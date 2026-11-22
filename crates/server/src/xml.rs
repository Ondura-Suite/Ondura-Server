//! Small XML tree for SOAP/POX requests. Element names are stored without
//! namespace prefixes (EWS and Autodiscover use unique local names).

use quick_xml::Reader;
use quick_xml::events::Event;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    match s.rsplit_once(':') {
        Some((_, l)) => l.to_string(),
        None => s.into_owned(),
    }
}

impl Node {
    pub fn parse(xml: &str) -> Result<Node, String> {
        let mut reader = Reader::from_str(xml);
        let mut stack: Vec<Node> = vec![Node { name: "#document".into(), ..Default::default() }];
        let attrs = |e: &quick_xml::events::BytesStart<'_>| -> Vec<(String, String)> {
            e.attributes()
                .flatten()
                .filter(|a| !a.key.as_ref().starts_with(b"xmlns"))
                .map(|a| (local(a.key.as_ref()), a.unescape_value().map(|v| v.into_owned()).unwrap_or_default()))
                .collect()
        };
        loop {
            match reader.read_event().map_err(|e| e.to_string())? {
                Event::Start(e) => stack.push(Node { name: local(e.name().as_ref()), attrs: attrs(&e), ..Default::default() }),
                Event::Empty(e) => {
                    let n = Node { name: local(e.name().as_ref()), attrs: attrs(&e), ..Default::default() };
                    stack.last_mut().ok_or("unbalanced XML")?.children.push(n);
                }
                Event::Text(t) => {
                    let text = t.decode().map_err(|e| e.to_string())?;
                    stack.last_mut().ok_or("unbalanced XML")?.text.push_str(&text);
                }
                Event::GeneralRef(r) => {
                    let resolved = match r.resolve_char_ref().map_err(|e| e.to_string())? {
                        Some(c) => c.to_string(),
                        None => match r.decode().map_err(|e| e.to_string())?.as_ref() {
                            "amp" => "&".into(),
                            "lt" => "<".into(),
                            "gt" => ">".into(),
                            "quot" => "\"".into(),
                            "apos" => "'".into(),
                            other => format!("&{other};"),
                        },
                    };
                    stack.last_mut().ok_or("unbalanced XML")?.text.push_str(&resolved);
                }
                Event::CData(t) => stack.last_mut().ok_or("unbalanced XML")?.text.push_str(&String::from_utf8_lossy(&t)),
                Event::End(_) => {
                    let n = stack.pop().ok_or("unbalanced XML")?;
                    stack.last_mut().ok_or("unbalanced XML")?.children.push(n);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if stack.len() != 1 {
            return Err("unexpected end of XML".into());
        }
        Ok(stack.pop().expect("document"))
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// Text of a child, trimmed.
    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    /// Untrimmed text of a child (message bodies).
    pub fn raw_text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }

    pub fn find(&self, name: &str) -> Option<&Node> {
        for c in &self.children {
            if c.name == name {
                return Some(c);
            }
            if let Some(f) = c.find(name) {
                return Some(f);
            }
        }
        None
    }

    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            }
            c.find_all(name, out);
        }
    }

}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Characters not allowed in XML 1.0.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_soap() {
        let x = r#"<?xml version="1.0"?><s:Envelope xmlns:s="x" xmlns:t="y"><s:Body><m:GetFolder><m:FolderIds><t:DistinguishedFolderId Id="inbox"/></m:FolderIds><t:Subject> A &amp; B </t:Subject></m:GetFolder></s:Body></s:Envelope>"#;
        let n = Node::parse(x).unwrap();
        let gf = n.find("GetFolder").unwrap();
        assert_eq!(gf.find("DistinguishedFolderId").unwrap().attr("Id"), Some("inbox"));
        assert_eq!(gf.text_of("Subject"), Some("A & B"));
        assert_eq!(gf.raw_text_of("Subject"), Some(" A & B "));
        assert_eq!(escape("a<b>&\u{1}"), "a&lt;b&gt;&amp;");
    }
}
