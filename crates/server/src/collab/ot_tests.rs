//! Tests for the OT engine, including a randomised convergence property.

use super::ot::{from_u16, to_u16, TextOp};
use serde_json::json;

fn op(base: &str, json: serde_json::Value) -> TextOp {
    TextOp::from_json(&json, to_u16(base).len() as u32).unwrap()
}

fn apply(doc: &str, o: &TextOp) -> String {
    from_u16(&o.apply(&to_u16(doc)).unwrap())
}

#[test]
fn apply_insert_delete_retain() {
    // "hello" -> retain 5, insert " world"
    let o = op("hello", json!([5, " world"]));
    assert_eq!(apply("hello", &o), "hello world");
    // delete first two, keep rest
    let o = op("hello", json!([-2, 3]));
    assert_eq!(apply("hello", &o), "llo");
}

#[test]
fn apply_rejects_wrong_base_length() {
    let o = op("hello", json!([5]));
    assert!(o.apply(&to_u16("hi")).is_err());
}

#[test]
fn utf16_counts_handle_emoji() {
    // "a😀b": the emoji is two UTF-16 units. Retain 1 (a) + 2 (emoji) = 3, then insert.
    let base = "a😀b";
    // retain a(1) + emoji(2) = 3, insert "!", retain b(1).
    let o = op(base, json!([3, "!", 1]));
    assert_eq!(apply(base, &o), "a😀!b");
}

#[test]
fn compose_sequential_edits() {
    // insert " world" then delete "hello " leaving "world"? compose and apply once.
    let a = op("hello", json!([5, " world"])); // -> "hello world" (len 11)
    let b = op("hello world", json!([-6, 5])); // delete "hello ", keep "world"
    let c = a.compose(&b).unwrap();
    assert_eq!(apply("hello", &c), "world");
}

#[test]
fn transform_converges_concurrent_inserts() {
    // Both edit "abc" concurrently: A inserts "X" at 0, B inserts "Y" at end.
    let a = op("abc", json!(["X", 3]));
    let b = op("abc", json!([3, "Y"]));
    let (a_prime, b_prime) = TextOp::transform(&a, &b).unwrap();
    // doc -> a -> b_prime  must equal  doc -> b -> a_prime
    let via_a = apply(&apply("abc", &a), &b_prime);
    let via_b = apply(&apply("abc", &b), &a_prime);
    assert_eq!(via_a, via_b);
    assert_eq!(via_a, "XabcY");
}

#[test]
fn transform_insert_same_position_deterministic() {
    let a = op("abc", json!([1, "A", 2]));
    let b = op("abc", json!([1, "B", 2]));
    let (a_prime, b_prime) = TextOp::transform(&a, &b).unwrap();
    let via_a = apply(&apply("abc", &a), &b_prime);
    let via_b = apply(&apply("abc", &b), &a_prime);
    assert_eq!(via_a, via_b);
    // a's insert comes first.
    assert_eq!(via_a, "aABbc");
}

#[test]
fn transform_concurrent_delete_overlap() {
    // A deletes "bc", B deletes "cd" from "abcde"; overlapping "c".
    let a = op("abcde", json!([1, -2, 2])); // delete bc -> "ade"
    let b = op("abcde", json!([2, -2, 1])); // delete cd -> "abe"
    let (a_prime, b_prime) = TextOp::transform(&a, &b).unwrap();
    let via_a = apply(&apply("abcde", &a), &b_prime);
    let via_b = apply(&apply("abcde", &b), &a_prime);
    assert_eq!(via_a, via_b);
    assert_eq!(via_a, "ae");
}

#[test]
fn from_json_rejects_out_of_range_counts() {
    // i64::MIN must not panic (it would overflow `-n`); huge counts are rejected.
    assert!(TextOp::from_json(&json!([i64::MIN]), 0).is_err());
    assert!(TextOp::from_json(&json!([5_000_000_000i64]), 0).is_err());
    assert!(TextOp::from_json(&json!([-5_000_000_000i64]), 0).is_err());
}

#[test]
fn roundtrip_json() {
    let o = op("hello", json!([2, "X", -1, 2]));
    let j = o.to_json();
    let o2 = TextOp::from_json(&j, 5).unwrap();
    assert_eq!(o, o2);
}

// --- randomised convergence property ---

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next() % n as u64) as usize
    }
}

/// Build a random operation over a document of `len` UTF-16 units.
fn random_op(rng: &mut Rng, len: u32) -> TextOp {
    let mut o = TextOp::new();
    let mut pos = 0u32;
    while pos < len {
        let remaining = len - pos;
        match rng.below(3) {
            0 => {
                // retain
                let n = 1 + rng.below(remaining as usize) as u32;
                o.retain(n);
                pos += n;
            }
            1 => {
                // insert
                let s: Vec<u16> = (0..1 + rng.below(3)).map(|_| b'a' as u16 + rng.below(5) as u16).collect();
                o.insert(&s);
            }
            _ => {
                // delete
                let n = 1 + rng.below(remaining as usize) as u32;
                o.delete(n);
                pos += n;
            }
        }
    }
    // Occasionally a trailing insert.
    if rng.below(2) == 0 {
        let s: Vec<u16> = (0..1 + rng.below(3)).map(|_| b'z' as u16).collect();
        o.insert(&s);
    }
    o
}

#[test]
fn parse_path_never_panics() {
    use std::panic::catch_unwind;
    let mut rng = Rng(0x5EED_1234_5678_9AB0);
    for _ in 0..20000 {
        // Build a random JSON "operation": a mix of integers (including extreme
        // and negative values) and short strings.
        let n = rng.below(8);
        let mut arr: Vec<serde_json::Value> = Vec::new();
        for _ in 0..n {
            match rng.below(5) {
                0 => arr.push(serde_json::json!(rng.below(130) as i64 - 8)),
                1 => arr.push(serde_json::json!(i64::MIN + rng.below(3) as i64)),
                2 => arr.push(serde_json::json!(i64::MAX - rng.below(3) as i64)),
                3 => arr.push(serde_json::json!("x".repeat(rng.below(4)))),
                _ => arr.push(serde_json::json!(-(rng.below(130) as i64))),
            }
        }
        let value = serde_json::Value::Array(arr);
        let r = catch_unwind(|| {
            for base in [0u32, 3, 64] {
                if let Ok(op) = TextOp::from_json(&value, base) {
                    let doc = to_u16(&"a".repeat(base as usize));
                    if let Ok(after) = op.apply(&doc) {
                        let mut noop = TextOp::new();
                        noop.retain(base);
                        if let Ok((_, b_prime)) = TextOp::transform(&op, &noop) {
                            let _ = b_prime.apply(&after);
                        }
                    }
                }
            }
        });
        assert!(r.is_ok(), "parse path panicked on {value:?}");
    }
}

#[test]
fn randomised_convergence() {
    let mut rng = Rng(0x0CAB_1234_5678_9ABC);
    for _ in 0..5000 {
        // Random starting document.
        let dlen = 1 + rng.below(12);
        let doc: String = (0..dlen).map(|_| (b'a' + rng.below(6) as u8) as char).collect();
        let base = to_u16(&doc).len() as u32;
        let a = random_op(&mut rng, base);
        let b = random_op(&mut rng, base);
        let (a_prime, b_prime) = match TextOp::transform(&a, &b) {
            Ok(p) => p,
            Err(e) => panic!("transform failed on '{doc}': {e}"),
        };
        let after_a = a.apply(&to_u16(&doc)).unwrap();
        let after_b = b.apply(&to_u16(&doc)).unwrap();
        let via_a = b_prime.apply(&after_a).unwrap_or_else(|e| panic!("b' apply: {e} doc='{doc}'"));
        let via_b = a_prime.apply(&after_b).unwrap_or_else(|e| panic!("a' apply: {e} doc='{doc}'"));
        assert_eq!(via_a, via_b, "diverged on doc '{doc}'");
    }
}
