//! WBXML (WAP Binary XML 1.3) codec with the Exchange ActiveSync code pages
//! ([MS-ASWBXML]).
//!
//! Documents are plain element trees. Element names are written as
//! `"CodePage:Tag"`, e.g. `"AirSync:Sync"` or `"FolderHierarchy:ServerId"`.
//! Lookups on [`Element`] accept either the qualified name or just the tag
//! name (`"Status"` matches a status element of any code page).

mod tables;

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::sync::OnceLock;

const SWITCH_PAGE: u8 = 0x00;
const END: u8 = 0x01;
const ENTITY: u8 = 0x02;
const STR_I: u8 = 0x03;
const LITERAL: u8 = 0x04;
const STR_T: u8 = 0x83;
const OPAQUE: u8 = 0xC3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnexpectedEnd,
    UnknownTag { page: u8, token: u8 },
    UnknownName(String),
    Unsupported(&'static str),
    InvalidUtf8,
    TooDeep,
}

/// Maximum element nesting the decoder accepts. ActiveSync payloads are only a
/// handful of levels deep; this cap keeps a hostile, deeply nested document from
/// overflowing the stack during recursive decoding.
const MAX_DEPTH: usize = 100;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEnd => write!(f, "unexpected end of WBXML data"),
            Error::UnknownTag { page, token } => write!(f, "unknown WBXML tag 0x{token:02x} on code page {page}"),
            Error::UnknownName(n) => write!(f, "no WBXML token for element {n}"),
            Error::Unsupported(what) => write!(f, "unsupported WBXML feature: {what}"),
            Error::InvalidUtf8 => write!(f, "invalid UTF-8 in WBXML string"),
            Error::TooDeep => write!(f, "WBXML document nested beyond the depth limit"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
    Opaque(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Code page index.
    pub page: u8,
    /// Tag name without the code page, e.g. `Sync`.
    pub tag: &'static str,
    pub children: Vec<Node>,
}

struct Codebook {
    by_name: HashMap<(&'static str, &'static str), (u8, u8)>,
}

fn codebook() -> &'static Codebook {
    static BOOK: OnceLock<Codebook> = OnceLock::new();
    BOOK.get_or_init(|| {
        let mut by_name = HashMap::new();
        for (page, (ns, tags)) in tables::PAGES.iter().enumerate() {
            for (i, tag) in tags.iter().enumerate() {
                if !tag.is_empty() {
                    by_name.insert((*ns, *tag), (page as u8, i as u8 + 5));
                }
            }
        }
        Codebook { by_name }
    })
}

/// Name of a code page (`"AirSync"` for 0).
pub fn page_name(page: u8) -> Option<&'static str> {
    tables::PAGES.get(page as usize).map(|(n, _)| *n)
}

fn tag_of(page: u8, token: u8) -> Option<&'static str> {
    let (_, tags) = tables::PAGES.get(page as usize)?;
    tags.get(token.checked_sub(5)? as usize).copied().filter(|t| !t.is_empty())
}

fn split(name: &str) -> (Option<&str>, &str) {
    match name.split_once(':') {
        Some((ns, tag)) => (Some(ns), tag),
        None => (None, name),
    }
}

impl Element {
    /// New element from a qualified name (`"AirSync:Sync"`). Panics on an
    /// unknown name: names are compile-time constants in this code base.
    pub fn new(name: &str) -> Element {
        Self::try_new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    pub fn try_new(name: &str) -> Result<Element> {
        let (ns, tag) = split(name);
        let ns = ns.ok_or_else(|| Error::UnknownName(name.to_string()))?;
        let &(page, token) = codebook().by_name.get(&(ns, tag)).ok_or_else(|| Error::UnknownName(name.to_string()))?;
        let tag = tag_of(page, token).expect("token from the code book");
        Ok(Element { page, tag, children: Vec::new() })
    }

    /// Qualified name, e.g. `AirSync:Sync`.
    pub fn name(&self) -> String {
        format!("{}:{}", page_name(self.page).unwrap_or("?"), self.tag)
    }

    pub fn is(&self, name: &str) -> bool {
        let (ns, tag) = split(name);
        self.tag == tag && ns.is_none_or(|ns| page_name(self.page) == Some(ns))
    }

    // ----------------------------------------------------------- building

    /// Add a child element (builder style).
    pub fn with(mut self, child: Element) -> Element {
        self.children.push(Node::Element(child));
        self
    }

    /// Add `<name>text</name>` (builder style).
    pub fn with_text(self, name: &str, text: impl Into<String>) -> Element {
        self.with(Element::new(name).text(text))
    }

    /// Add an empty `<name/>` (builder style).
    pub fn with_empty(self, name: &str) -> Element {
        self.with(Element::new(name))
    }

    /// Set text content (builder style).
    pub fn text(mut self, text: impl Into<String>) -> Element {
        let text = text.into();
        if !text.is_empty() {
            self.children.push(Node::Text(text));
        }
        self
    }

    pub fn opaque(mut self, data: Vec<u8>) -> Element {
        self.children.push(Node::Opaque(data));
        self
    }

    pub fn push(&mut self, child: Element) {
        self.children.push(Node::Element(child));
    }

    pub fn push_text(&mut self, name: &str, text: impl Into<String>) {
        self.push(Element::new(name).text(text));
    }

    // ------------------------------------------------------------ reading

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            _ => None,
        })
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.is(name))
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.elements().filter(move |e| e.is(name))
    }

    /// Child by path, e.g. `"Collections/Collection"`.
    pub fn path(&self, path: &str) -> Option<&Element> {
        path.split('/').try_fold(self, |e, p| e.child(p))
    }

    /// Text content of this element (text and opaque data as UTF-8).
    pub fn content(&self) -> String {
        let mut s = String::new();
        for c in &self.children {
            match c {
                Node::Text(t) => s.push_str(t),
                Node::Opaque(o) => s.push_str(&String::from_utf8_lossy(o)),
                Node::Element(_) => {}
            }
        }
        s
    }

    /// Raw bytes of the content (opaque data or text).
    pub fn bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for c in &self.children {
            match c {
                Node::Text(t) => v.extend_from_slice(t.as_bytes()),
                Node::Opaque(o) => v.extend_from_slice(o),
                Node::Element(_) => {}
            }
        }
        v
    }

    pub fn text_of(&self, name: &str) -> Option<String> {
        self.child(name).map(|e| e.content())
    }

    pub fn has(&self, name: &str) -> bool {
        self.child(name).is_some()
    }

    /// Depth-first search for the first descendant with this name.
    pub fn find(&self, name: &str) -> Option<&Element> {
        for e in self.elements() {
            if e.is(name) {
                return Some(e);
            }
            if let Some(f) = e.find(name) {
                return Some(f);
            }
        }
        None
    }

    // ------------------------------------------------------------- coding

    /// Encode as a WBXML document.
    pub fn encode(&self) -> Vec<u8> {
        // Version 1.3, unknown public id, UTF-8, empty string table.
        let mut out = vec![0x03, 0x01, 0x6a, 0x00];
        let mut page = 0u8;
        self.encode_into(&mut out, &mut page);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>, page: &mut u8) {
        let ns = page_name(self.page).unwrap_or_default();
        let (_, token) = codebook().by_name[&(ns, self.tag)];
        if *page != self.page {
            out.push(SWITCH_PAGE);
            out.push(self.page);
            *page = self.page;
        }
        if self.children.is_empty() {
            out.push(token);
            return;
        }
        out.push(token | 0x40);
        for c in &self.children {
            match c {
                Node::Element(e) => e.encode_into(out, page),
                Node::Text(t) => {
                    out.push(STR_I);
                    out.extend_from_slice(t.as_bytes());
                    out.push(0);
                }
                Node::Opaque(o) => {
                    out.push(OPAQUE);
                    write_mb_u32(out, o.len() as u32);
                    out.extend_from_slice(o);
                }
            }
        }
        out.push(END);
    }

    /// Decode a WBXML document.
    pub fn decode(data: &[u8]) -> Result<Element> {
        let mut r = Reader { data, pos: 0 };
        let _version = r.byte()?;
        let public_id = r.mb_u32()?;
        if public_id == 0 {
            r.mb_u32()?; // string table index of the public id
        }
        let _charset = r.mb_u32()?;
        let strtbl_len = r.mb_u32()? as usize;
        let strtbl = r.take(strtbl_len)?.to_vec();
        let mut page = 0u8;
        // Skip anything before the root element (processing instructions are not used by EAS).
        loop {
            let b = r.peek()?;
            if b == SWITCH_PAGE {
                r.byte()?;
                page = r.byte()?;
            } else {
                break;
            }
        }
        r.element(&mut page, &strtbl, 0)
    }

    /// Indented XML rendering, for logs and tests.
    pub fn to_xml(&self) -> String {
        let mut s = String::new();
        self.xml_into(&mut s, 0);
        s
    }

    fn xml_into(&self, s: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        let name = self.name();
        if self.children.is_empty() {
            let _ = writeln!(s, "{pad}<{name}/>");
            return;
        }
        if self.children.iter().all(|c| !matches!(c, Node::Element(_))) {
            let text: String = self
                .children
                .iter()
                .map(|c| match c {
                    Node::Text(t) => xml_escape(t),
                    Node::Opaque(o) => format!("[{} bytes]", o.len()),
                    Node::Element(_) => String::new(),
                })
                .collect();
            let _ = writeln!(s, "{pad}<{name}>{text}</{name}>");
            return;
        }
        let _ = writeln!(s, "{pad}<{name}>");
        for c in &self.children {
            match c {
                Node::Element(e) => e.xml_into(s, depth + 1),
                Node::Text(t) => {
                    let _ = writeln!(s, "{pad}  {}", xml_escape(t));
                }
                Node::Opaque(o) => {
                    let _ = writeln!(s, "{pad}  [{} bytes]", o.len());
                }
            }
        }
        let _ = writeln!(s, "{pad}</{name}>");
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn write_mb_u32(out: &mut Vec<u8>, mut v: u32) {
    let mut buf = [0u8; 5];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    let last = buf.len() - 1;
    for b in &mut buf[i..last] {
        *b |= 0x80;
    }
    out.extend_from_slice(&buf[i..]);
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn peek(&self) -> Result<u8> {
        self.data.get(self.pos).copied().ok_or(Error::UnexpectedEnd)
    }

    fn byte(&mut self) -> Result<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.data.len()).ok_or(Error::UnexpectedEnd)?;
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn mb_u32(&mut self) -> Result<u32> {
        let mut v: u32 = 0;
        for _ in 0..5 {
            let b = self.byte()?;
            v = (v << 7) | (b & 0x7f) as u32;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(Error::Unsupported("multi-byte integer longer than 5 bytes"))
    }

    fn cstring(&mut self) -> Result<String> {
        let rest = &self.data[self.pos..];
        let len = rest.iter().position(|b| *b == 0).ok_or(Error::UnexpectedEnd)?;
        let s = std::str::from_utf8(&rest[..len]).map_err(|_| Error::InvalidUtf8)?.to_string();
        self.pos += len + 1;
        Ok(s)
    }

    fn element(&mut self, page: &mut u8, strtbl: &[u8], depth: usize) -> Result<Element> {
        if depth >= MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        let b = self.byte()?;
        if b & 0x80 != 0 {
            return Err(Error::Unsupported("attributes"));
        }
        let token = b & 0x3f;
        let tag = tag_of(*page, token).ok_or(Error::UnknownTag { page: *page, token })?;
        let mut el = Element { page: *page, tag, children: Vec::new() };
        if b & 0x40 == 0 {
            return Ok(el);
        }
        loop {
            match self.peek()? {
                END => {
                    self.byte()?;
                    return Ok(el);
                }
                SWITCH_PAGE => {
                    self.byte()?;
                    *page = self.byte()?;
                }
                STR_I => {
                    self.byte()?;
                    let s = self.cstring()?;
                    push_text(&mut el, s);
                }
                STR_T => {
                    self.byte()?;
                    let idx = self.mb_u32()? as usize;
                    let rest = strtbl.get(idx..).ok_or(Error::UnexpectedEnd)?;
                    let len = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
                    let s = std::str::from_utf8(&rest[..len]).map_err(|_| Error::InvalidUtf8)?.to_string();
                    push_text(&mut el, s);
                }
                ENTITY => {
                    self.byte()?;
                    let c = self.mb_u32()?;
                    push_text(&mut el, char::from_u32(c).map(String::from).unwrap_or_default());
                }
                OPAQUE => {
                    self.byte()?;
                    let len = self.mb_u32()? as usize;
                    let data = self.take(len)?.to_vec();
                    el.children.push(Node::Opaque(data));
                }
                LITERAL => return Err(Error::Unsupported("literal tags")),
                0x40..=0x44 | 0x80..=0x82 | 0xc0..=0xc2 => return Err(Error::Unsupported("extension tokens")),
                _ => {
                    let child = self.element(page, strtbl, depth + 1)?;
                    el.children.push(Node::Element(child));
                }
            }
        }
    }
}

fn push_text(el: &mut Element, s: String) {
    if let Some(Node::Text(t)) = el.children.last_mut() {
        t.push_str(&s);
    } else {
        el.children.push(Node::Text(s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let doc = Element::new("FolderHierarchy:FolderSync").with_text("FolderHierarchy:SyncKey", "0");
        let bytes = doc.encode();
        // Header, SWITCH_PAGE 7, FolderSync with content, SyncKey with content, "0".
        assert_eq!(bytes, vec![0x03, 0x01, 0x6a, 0x00, 0x00, 0x07, 0x56, 0x52, 0x03, b'0', 0x00, 0x01, 0x01]);
        assert_eq!(Element::decode(&bytes).unwrap(), doc);
    }

    #[test]
    fn mixed_pages_and_opaque() {
        let doc = Element::new("AirSync:Sync").with(
            Element::new("AirSync:Collections").with(
                Element::new("AirSync:Collection")
                    .with_text("AirSync:SyncKey", "1")
                    .with(Element::new("AirSyncBase:Body").with_text("AirSyncBase:Type", "2").with(Element::new("AirSyncBase:Data").opaque(b"<p>Hi</p>".to_vec())))
                    .with_text("AirSync:Status", "1"),
            ),
        );
        let back = Element::decode(&doc.encode()).unwrap();
        assert_eq!(back, doc);
        let col = back.path("Collections/Collection").unwrap();
        assert_eq!(col.text_of("Status").as_deref(), Some("1"));
        assert_eq!(col.find("Data").unwrap().content(), "<p>Hi</p>");
        assert!(col.child("AirSyncBase:Body").is_some());
        assert!(col.child("Email:Body").is_none());
    }

    #[test]
    fn rejects_deeply_nested() {
        // A hand-built document nested far past MAX_DEPTH must be refused rather
        // than overflowing the stack. Each element is "FolderSync" with content
        // (0x6a) followed by its single child; depth is closed with END bytes.
        let mut bytes = vec![0x03u8, 0x01, 0x6a, 0x00, 0x00, 0x07];
        let depth = MAX_DEPTH + 50;
        bytes.resize(bytes.len() + depth, 0x56); // FolderSync, with content
        bytes.resize(bytes.len() + depth, 0x01); // END
        assert!(matches!(Element::decode(&bytes), Err(Error::TooDeep)));
    }

    #[test]
    fn multibyte_integers() {
        let mut v = Vec::new();
        write_mb_u32(&mut v, 0xA0);
        assert_eq!(v, vec![0x81, 0x20]);
        let mut r = Reader { data: &v, pos: 0 };
        assert_eq!(r.mb_u32().unwrap(), 0xA0);
    }

    #[test]
    fn known_tokens() {
        // Spot checks against [MS-ASWBXML].
        let check = |name: &str, page: u8, token: u8| {
            let (ns, tag) = split(name);
            assert_eq!(codebook().by_name[&(ns.unwrap(), tag)], (page, token), "{name}");
        };
        check("AirSync:Sync", 0, 0x05);
        check("AirSync:HeartbeatInterval", 0, 0x29);
        check("Email:Read", 2, 0x15);
        check("Calendar:UID", 4, 0x28);
        check("FolderHierarchy:FolderSync", 7, 0x16);
        check("Ping:HeartbeatInterval", 13, 0x08);
        check("Provision:EASProvisionDoc", 14, 0x0d);
        check("AirSyncBase:Preview", 17, 0x18);
        check("Settings:PrimarySmtpAddress", 18, 0x23);
        check("ComposeMail:MIME", 21, 0x10);
    }

    #[test]
    fn rejects_truncated_input() {
        assert_eq!(Element::decode(&[0x03, 0x01, 0x6a, 0x00, 0x05]).unwrap().tag, "Sync");
        assert!(Element::decode(&[0x03, 0x01, 0x6a, 0x00, 0x45]).is_err());
    }
}
