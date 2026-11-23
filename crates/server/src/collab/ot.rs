//! Operational Transform for collaborative plain-text editing.
//!
//! An operation is a sequence of components applied left to right over a
//! document: retain `n` units, insert a string, or delete `n` units. Lengths are
//! counted in UTF-16 code units so they match JavaScript string offsets exactly
//! (a browser client and this server must agree on positions, emoji included).
//!
//! The algorithm follows the well-known `ot.js` model. `transform` takes two
//! operations made against the same document and returns a prime pair such that
//! `a.compose(b_prime) == b.compose(a_prime)` as document transforms, so applying
//! either side's operation followed by the other's prime reaches one document.
//! Concurrent inserts at the same position are ordered deterministically (the
//! insert from `a` comes first).

use std::collections::VecDeque;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Comp {
    Retain(u32),
    Insert(Vec<u16>),
    Delete(u32),
}

/// A text operation: components plus the document lengths it maps between.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextOp {
    comps: Vec<Comp>,
    base_len: u32,
    target_len: u32,
}

impl TextOp {
    pub fn new() -> Self {
        TextOp::default()
    }

    pub fn base_len(&self) -> u32 {
        self.base_len
    }

    pub fn target_len(&self) -> u32 {
        self.target_len
    }

    pub fn is_noop(&self) -> bool {
        self.comps.iter().all(|c| matches!(c, Comp::Retain(_)))
    }

    pub fn retain(&mut self, n: u32) -> &mut Self {
        if n == 0 {
            return self;
        }
        self.base_len += n;
        self.target_len += n;
        if let Some(Comp::Retain(last)) = self.comps.last_mut() {
            *last += n;
        } else {
            self.comps.push(Comp::Retain(n));
        }
        self
    }

    pub fn insert(&mut self, s: &[u16]) -> &mut Self {
        if s.is_empty() {
            return self;
        }
        self.target_len += s.len() as u32;
        match self.comps.last_mut() {
            Some(Comp::Insert(prev)) => prev.extend_from_slice(s),
            // Keep an insert before an adjacent delete (canonical ot.js order).
            Some(Comp::Delete(_)) => {
                let del = self.comps.pop().expect("delete present");
                if let Some(Comp::Insert(prev)) = self.comps.last_mut() {
                    prev.extend_from_slice(s);
                } else {
                    self.comps.push(Comp::Insert(s.to_vec()));
                }
                self.comps.push(del);
            }
            _ => self.comps.push(Comp::Insert(s.to_vec())),
        }
        self
    }

    pub fn delete(&mut self, n: u32) -> &mut Self {
        if n == 0 {
            return self;
        }
        self.base_len += n;
        if let Some(Comp::Delete(last)) = self.comps.last_mut() {
            *last += n;
        } else {
            self.comps.push(Comp::Delete(n));
        }
        self
    }

    /// Apply the operation to a UTF-16 document, returning the new document.
    pub fn apply(&self, doc: &[u16]) -> Result<Vec<u16>, String> {
        if doc.len() as u32 != self.base_len {
            return Err(format!("operation base length {} does not match document length {}", self.base_len, doc.len()));
        }
        let mut out = Vec::with_capacity(self.target_len as usize);
        let mut i = 0usize;
        for c in &self.comps {
            match c {
                Comp::Retain(n) => {
                    let end = i + *n as usize;
                    if end > doc.len() {
                        return Err("retain past end of document".into());
                    }
                    out.extend_from_slice(&doc[i..end]);
                    i = end;
                }
                Comp::Insert(s) => out.extend_from_slice(s),
                Comp::Delete(n) => {
                    i += *n as usize;
                    if i > doc.len() {
                        return Err("delete past end of document".into());
                    }
                }
            }
        }
        if i != doc.len() {
            return Err("operation did not consume the whole document".into());
        }
        Ok(out)
    }

    /// Compose two operations applied in sequence (self, then `other`).
    pub fn compose(&self, other: &TextOp) -> Result<TextOp, String> {
        if self.target_len != other.base_len {
            return Err("compose length mismatch".into());
        }
        let mut res = TextOp::new();
        let mut a: VecDeque<Comp> = self.comps.iter().cloned().collect();
        let mut b: VecDeque<Comp> = other.comps.iter().cloned().collect();
        loop {
            match (a.front().cloned(), b.front().cloned()) {
                (None, None) => break,
                // A delete in the first op is final regardless of the second.
                (Some(Comp::Delete(n)), _) => {
                    res.delete(n);
                    a.pop_front();
                }
                // An insert in the second op is new regardless of the first.
                (_, Some(Comp::Insert(s))) => {
                    res.insert(&s);
                    b.pop_front();
                }
                (Some(Comp::Retain(n1)), Some(Comp::Retain(n2))) => {
                    let m = n1.min(n2);
                    res.retain(m);
                    consume(&mut a, n1, m);
                    consume(&mut b, n2, m);
                }
                (Some(Comp::Retain(n1)), Some(Comp::Delete(n2))) => {
                    let m = n1.min(n2);
                    res.delete(m);
                    consume(&mut a, n1, m);
                    consume(&mut b, n2, m);
                }
                (Some(Comp::Insert(s)), Some(Comp::Retain(n2))) => {
                    let m = (s.len() as u32).min(n2);
                    res.insert(&s[..m as usize]);
                    consume_insert(&mut a, &s, m);
                    consume(&mut b, n2, m);
                }
                (Some(Comp::Insert(s)), Some(Comp::Delete(n2))) => {
                    // Inserted text deleted again cancels out.
                    let m = (s.len() as u32).min(n2);
                    consume_insert(&mut a, &s, m);
                    consume(&mut b, n2, m);
                }
                (None, _) | (_, None) => return Err("compose ran off the end".into()),
            }
        }
        Ok(res)
    }

    /// Transform two operations made against the same base document. Returns
    /// `(a_prime, b_prime)`.
    pub fn transform(a_op: &TextOp, b_op: &TextOp) -> Result<(TextOp, TextOp), String> {
        if a_op.base_len != b_op.base_len {
            return Err("transform base length mismatch".into());
        }
        let mut a_prime = TextOp::new();
        let mut b_prime = TextOp::new();
        let mut a: VecDeque<Comp> = a_op.comps.iter().cloned().collect();
        let mut b: VecDeque<Comp> = b_op.comps.iter().cloned().collect();
        loop {
            match (a.front().cloned(), b.front().cloned()) {
                (None, None) => break,
                // Inserts from a come first.
                (Some(Comp::Insert(s)), _) => {
                    a_prime.insert(&s);
                    b_prime.retain(s.len() as u32);
                    a.pop_front();
                }
                (_, Some(Comp::Insert(s))) => {
                    a_prime.retain(s.len() as u32);
                    b_prime.insert(&s);
                    b.pop_front();
                }
                (Some(Comp::Retain(n1)), Some(Comp::Retain(n2))) => {
                    let m = n1.min(n2);
                    a_prime.retain(m);
                    b_prime.retain(m);
                    consume(&mut a, n1, m);
                    consume(&mut b, n2, m);
                }
                (Some(Comp::Delete(n1)), Some(Comp::Delete(n2))) => {
                    let m = n1.min(n2);
                    consume(&mut a, n1, m);
                    consume(&mut b, n2, m);
                }
                (Some(Comp::Delete(n1)), Some(Comp::Retain(n2))) => {
                    let m = n1.min(n2);
                    a_prime.delete(m);
                    consume(&mut a, n1, m);
                    consume(&mut b, n2, m);
                }
                (Some(Comp::Retain(n1)), Some(Comp::Delete(n2))) => {
                    let m = n1.min(n2);
                    b_prime.delete(m);
                    consume(&mut a, n1, m);
                    consume(&mut b, n2, m);
                }
                (None, _) | (_, None) => return Err("transform ran off the end".into()),
            }
        }
        Ok((a_prime, b_prime))
    }

    /// Parse the compact wire format: a JSON array whose elements are a positive
    /// number (retain), a negative number (delete) or a string (insert).
    pub fn from_json(v: &Value, base_len: u32) -> Result<TextOp, String> {
        let arr = v.as_array().ok_or("operation must be an array")?;
        let mut op = TextOp::new();
        for item in arr {
            if let Some(n) = item.as_i64() {
                // Reject counts outside u32 range rather than truncating or
                // overflowing on a hostile value such as i64::MIN.
                if n.unsigned_abs() > u32::MAX as u64 {
                    return Err("operation count out of range".into());
                }
                if n > 0 {
                    op.retain(n as u32);
                } else if n < 0 {
                    op.delete(n.unsigned_abs() as u32);
                }
            } else if let Some(s) = item.as_str() {
                op.insert(&to_u16(s));
            } else {
                return Err("invalid operation component".into());
            }
        }
        if op.base_len != base_len {
            return Err(format!("operation base length {} does not match revision length {base_len}", op.base_len));
        }
        Ok(op)
    }

    /// Serialise to the compact wire format.
    pub fn to_json(&self) -> Value {
        let mut arr = Vec::new();
        for c in &self.comps {
            match c {
                Comp::Retain(n) => arr.push(Value::from(*n)),
                Comp::Delete(n) => arr.push(Value::from(-(*n as i64))),
                Comp::Insert(s) => arr.push(Value::from(from_u16(s))),
            }
        }
        Value::Array(arr)
    }
}

/// Drop `m` units from the front retain/delete component `cur` of `q`, leaving
/// the remainder if any.
fn consume(q: &mut VecDeque<Comp>, n: u32, m: u32) {
    if m < n {
        let reduced = match q.front().expect("front present") {
            Comp::Retain(_) => Comp::Retain(n - m),
            Comp::Delete(_) => Comp::Delete(n - m),
            Comp::Insert(s) => Comp::Insert(s.clone()),
        };
        *q.front_mut().expect("front present") = reduced;
    } else {
        q.pop_front();
    }
}

/// Drop the first `m` units of a front insert component.
fn consume_insert(q: &mut VecDeque<Comp>, s: &[u16], m: u32) {
    if m < s.len() as u32 {
        *q.front_mut().expect("front present") = Comp::Insert(s[m as usize..].to_vec());
    } else {
        q.pop_front();
    }
}

pub fn to_u16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub fn from_u16(v: &[u16]) -> String {
    String::from_utf16_lossy(v)
}
