//! A small Markdown dialect for collaborative documents, used for the rendered
//! preview and for export to HTML and Word. It covers headings, paragraphs,
//! bullet and numbered lists, quotes, fenced code, horizontal rules, and inline
//! bold, italic, code and links. Line breaks inside a paragraph are kept, as a
//! plain-text editor's users expect. Links are only made for `http`, `https`
//! and `mailto` addresses.

#[derive(Debug, Clone, PartialEq)]
pub enum Inline {
    Text(String),
    Bold(Vec<Inline>),
    Italic(Vec<Inline>),
    Code(String),
    Link(Vec<Inline>, String),
    Break,
    /// A task-list box at the start of a list item (`- [ ]` / `- [x]`).
    Check(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Heading(u8, Vec<Inline>),
    Para(Vec<Inline>),
    Bullets(Vec<Vec<Inline>>),
    Numbers(Vec<Vec<Inline>>),
    Quote(Vec<Inline>),
    Code(String),
    Rule,
}

/// Split a document into blocks.
pub fn parse(text: &str) -> Vec<Block> {
    #[derive(PartialEq)]
    enum Open {
        None,
        Para,
        Bullets,
        Numbers,
        Quote,
    }
    let mut out = Vec::new();
    let mut open = Open::None;
    let mut lines: Vec<String> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    let mut code: Option<Vec<&str>> = None;

    let flush = |open: &mut Open, lines: &mut Vec<String>, items: &mut Vec<String>, out: &mut Vec<Block>| {
        match open {
            Open::Para => out.push(Block::Para(inline(&lines.join("\n")))),
            Open::Quote => out.push(Block::Quote(inline(&lines.join("\n")))),
            Open::Bullets => out.push(Block::Bullets(items.iter().map(|i| list_item(i)).collect())),
            Open::Numbers => out.push(Block::Numbers(items.iter().map(|i| list_item(i)).collect())),
            Open::None => {}
        }
        *open = Open::None;
        lines.clear();
        items.clear();
    };

    for raw in text.lines() {
        let line = raw.trim_end();
        if let Some(buf) = &mut code {
            if line.trim_start().starts_with("```") {
                out.push(Block::Code(buf.join("\n")));
                code = None;
            } else {
                buf.push(raw);
            }
            continue;
        }
        let t = line.trim_start();
        if t.starts_with("```") {
            flush(&mut open, &mut lines, &mut items, &mut out);
            code = Some(Vec::new());
            continue;
        }
        if t.is_empty() {
            flush(&mut open, &mut lines, &mut items, &mut out);
            continue;
        }
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            flush(&mut open, &mut lines, &mut items, &mut out);
            out.push(Block::Heading(hashes as u8, inline(t[hashes..].trim())));
            continue;
        }
        if t.len() >= 3 && ["-", "*", "_"].iter().any(|m| t.chars().all(|c| c.to_string() == *m)) {
            flush(&mut open, &mut lines, &mut items, &mut out);
            out.push(Block::Rule);
            continue;
        }
        if let Some(rest) = ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m)) {
            if open != Open::Bullets {
                flush(&mut open, &mut lines, &mut items, &mut out);
                open = Open::Bullets;
            }
            items.push(rest.to_string());
            continue;
        }
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && digits <= 9 && (t[digits..].starts_with(". ") || t[digits..].starts_with(") ")) {
            if open != Open::Numbers {
                flush(&mut open, &mut lines, &mut items, &mut out);
                open = Open::Numbers;
            }
            items.push(t[digits + 2..].to_string());
            continue;
        }
        if let Some(rest) = t.strip_prefix('>') {
            if open != Open::Quote {
                flush(&mut open, &mut lines, &mut items, &mut out);
                open = Open::Quote;
            }
            lines.push(rest.trim_start().to_string());
            continue;
        }
        match open {
            // A plain line right after a list item continues that item.
            Open::Bullets | Open::Numbers => {
                if let Some(last) = items.last_mut() {
                    last.push('\n');
                    last.push_str(t);
                }
            }
            Open::Quote => lines.push(t.to_string()),
            Open::Para => lines.push(line.to_string()),
            Open::None => {
                open = Open::Para;
                lines.push(line.to_string());
            }
        }
    }
    if let Some(buf) = code {
        out.push(Block::Code(buf.join("\n")));
    }
    flush(&mut open, &mut lines, &mut items, &mut out);
    out
}

/// A list item: inline formatting, after a task box if it starts with one.
fn list_item(text: &str) -> Vec<Inline> {
    for (prefix, done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
        if let Some(rest) = text.strip_prefix(prefix) {
            let mut v = vec![Inline::Check(done)];
            v.extend(inline(rest));
            return v;
        }
    }
    inline(text)
}

/// Parse inline formatting.
pub fn inline(text: &str) -> Vec<Inline> {
    let chars: Vec<char> = text.chars().collect();
    parse_inline(&chars)
}

fn parse_inline(c: &[char]) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    let mut buf = String::new();
    let push_text = |buf: &mut String, out: &mut Vec<Inline>| {
        if !buf.is_empty() {
            out.push(Inline::Text(std::mem::take(buf)));
        }
    };
    let find = |from: usize, pat: &[char]| (from..c.len().saturating_sub(pat.len() - 1)).find(|&j| c[j..j + pat.len()] == *pat);
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch == '\\' && i + 1 < c.len() && "\\`*_[]()#+-.!>".contains(c[i + 1]) {
            buf.push(c[i + 1]);
            i += 2;
            continue;
        }
        if ch == '\n' {
            push_text(&mut buf, &mut out);
            out.push(Inline::Break);
            i += 1;
            continue;
        }
        if ch == '`'
            && let Some(j) = find(i + 1, &['`'])
        {
            push_text(&mut buf, &mut out);
            out.push(Inline::Code(c[i + 1..j].iter().collect()));
            i = j + 1;
            continue;
        }
        if (ch == '*' || ch == '_') && i + 1 < c.len() && c[i + 1] == ch {
            let pat = [ch, ch];
            if let Some(j) = find(i + 2, &pat).filter(|&j| j > i + 2) {
                push_text(&mut buf, &mut out);
                out.push(Inline::Bold(parse_inline(&c[i + 2..j])));
                i = j + 2;
                continue;
            }
        }
        // `_` only opens emphasis at a word boundary, so snake_case stays text.
        let boundary = ch == '*' || i == 0 || !c[i - 1].is_alphanumeric();
        if (ch == '*' || ch == '_')
            && boundary
            && i + 1 < c.len()
            && !c[i + 1].is_whitespace()
            && let Some(j) = find(i + 1, &[ch]).filter(|&j| j > i + 1 && !c[j - 1].is_whitespace())
        {
            push_text(&mut buf, &mut out);
            out.push(Inline::Italic(parse_inline(&c[i + 1..j])));
            i = j + 1;
            continue;
        }
        if ch == '['
            && let Some(close) = find(i + 1, &[']'])
            && close + 1 < c.len()
            && c[close + 1] == '('
            && let Some(end) = find(close + 2, &[')'])
        {
            let url: String = c[close + 2..end].iter().collect::<String>().trim().to_string();
            let lower = url.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
                push_text(&mut buf, &mut out);
                out.push(Inline::Link(parse_inline(&c[i + 1..close]), url));
                i = end + 1;
                continue;
            }
        }
        buf.push(ch);
        i += 1;
    }
    push_text(&mut buf, &mut out);
    out
}

// ------------------------------------------------------------------ HTML

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn inline_html(v: &[Inline]) -> String {
    let mut s = String::new();
    for i in v {
        match i {
            Inline::Text(t) => s.push_str(&esc(t)),
            Inline::Bold(x) => s.push_str(&format!("<strong>{}</strong>", inline_html(x))),
            Inline::Italic(x) => s.push_str(&format!("<em>{}</em>", inline_html(x))),
            Inline::Code(t) => s.push_str(&format!("<code>{}</code>", esc(t))),
            Inline::Link(x, url) => s.push_str(&format!("<a href=\"{}\" rel=\"noopener noreferrer\" target=\"_blank\">{}</a>", esc(url), inline_html(x))),
            Inline::Break => s.push_str("<br>"),
            Inline::Check(done) => s.push_str(if *done { "<input type=\"checkbox\" checked disabled> " } else { "<input type=\"checkbox\" disabled> " }),
        }
    }
    s
}

/// The document as an HTML fragment (every piece of text escaped).
pub fn to_html(blocks: &[Block]) -> String {
    let mut s = String::new();
    for b in blocks {
        match b {
            Block::Heading(n, x) => s.push_str(&format!("<h{n}>{}</h{n}>\n", inline_html(x))),
            Block::Para(x) => s.push_str(&format!("<p>{}</p>\n", inline_html(x))),
            Block::Quote(x) => s.push_str(&format!("<blockquote>{}</blockquote>\n", inline_html(x))),
            Block::Code(t) => s.push_str(&format!("<pre><code>{}</code></pre>\n", esc(t))),
            Block::Rule => s.push_str("<hr>\n"),
            Block::Bullets(items) | Block::Numbers(items) => {
                let tag = if matches!(b, Block::Bullets(_)) { "ul" } else { "ol" };
                s.push_str(&format!("<{tag}>"));
                for it in items {
                    s.push_str(&format!("<li>{}</li>", inline_html(it)));
                }
                s.push_str(&format!("</{tag}>\n"));
            }
        }
    }
    s
}

/// A standalone HTML page for download.
pub fn to_html_page(title: &str, blocks: &[Block]) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>{}</title><style>body{{font:16px/1.55 system-ui,sans-serif;max-width:46rem;margin:2rem auto;padding:0 1rem;color:#1f2328}}pre{{background:#f6f8fa;padding:.8rem;border-radius:6px;overflow:auto}}code{{font-family:ui-monospace,Consolas,monospace}}blockquote{{border-left:3px solid #d0d7de;margin:0;padding-left:1rem;color:#57606a}}</style></head><body>\n{}</body></html>\n",
        esc(title),
        to_html(blocks)
    )
}

// ------------------------------------------------------------------ Word

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
        .chars()
        // XML 1.0 forbids most control characters.
        .filter(|c| !c.is_control() || *c == '\t')
        .collect()
}

#[derive(Default, Clone, Copy)]
struct Fmt {
    bold: bool,
    italic: bool,
    code: bool,
    link: bool,
}

struct Doc {
    links: Vec<String>,
}

impl Doc {
    fn runs(&mut self, v: &[Inline], f: Fmt, out: &mut String) {
        for i in v {
            match i {
                Inline::Text(t) => out.push_str(&run(t, f)),
                Inline::Code(t) => out.push_str(&run(t, Fmt { code: true, ..f })),
                Inline::Bold(x) => self.runs(x, Fmt { bold: true, ..f }, out),
                Inline::Italic(x) => self.runs(x, Fmt { italic: true, ..f }, out),
                Inline::Break => out.push_str("<w:r><w:br/></w:r>"),
                Inline::Check(done) => out.push_str(&run(if *done { "\u{2612} " } else { "\u{2610} " }, f)),
                Inline::Link(x, url) => {
                    self.links.push(url.clone());
                    let id = format!("rIdL{}", self.links.len());
                    out.push_str(&format!("<w:hyperlink r:id=\"{id}\">"));
                    self.runs(x, Fmt { link: true, ..f }, out);
                    out.push_str("</w:hyperlink>");
                }
            }
        }
    }

    fn para(&mut self, style: Option<&str>, prefix: Option<&str>, v: &[Inline], out: &mut String) {
        out.push_str("<w:p>");
        if let Some(st) = style {
            out.push_str(&format!("<w:pPr><w:pStyle w:val=\"{st}\"/></w:pPr>"));
        }
        if let Some(p) = prefix {
            out.push_str(&run(p, Fmt::default()));
        }
        self.runs(v, Fmt::default(), out);
        out.push_str("</w:p>");
    }
}

fn run(t: &str, f: Fmt) -> String {
    let mut pr = String::new();
    if f.bold {
        pr.push_str("<w:b/>");
    }
    if f.italic {
        pr.push_str("<w:i/>");
    }
    if f.code {
        pr.push_str("<w:rFonts w:ascii=\"Consolas\" w:hAnsi=\"Consolas\" w:cs=\"Consolas\"/>");
    }
    if f.link {
        pr.push_str("<w:color w:val=\"0563C1\"/><w:u w:val=\"single\"/>");
    }
    let pr = if pr.is_empty() { String::new() } else { format!("<w:rPr>{pr}</w:rPr>") };
    format!("<w:r>{pr}<w:t xml:space=\"preserve\">{}</w:t></w:r>", xml(t))
}

const STYLES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:cs="Calibri"/><w:sz w:val="22"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120" w:line="276" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults>
<w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style>
<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="240"/></w:pPr><w:rPr><w:b/><w:sz w:val="44"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="240" w:after="120"/><w:outlineLvl w:val="0"/></w:pPr><w:rPr><w:b/><w:sz w:val="36"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading2"><w:name w:val="heading 2"/><w:basedOn w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="200" w:after="100"/><w:outlineLvl w:val="1"/></w:pPr><w:rPr><w:b/><w:sz w:val="30"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading3"><w:name w:val="heading 3"/><w:basedOn w:val="Normal"/><w:pPr><w:keepNext/><w:spacing w:before="160" w:after="80"/><w:outlineLvl w:val="2"/></w:pPr><w:rPr><w:b/><w:sz w:val="26"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading4"><w:name w:val="heading 4"/><w:basedOn w:val="Normal"/><w:pPr><w:keepNext/><w:outlineLvl w:val="3"/></w:pPr><w:rPr><w:b/><w:i/><w:sz w:val="24"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading5"><w:name w:val="heading 5"/><w:basedOn w:val="Normal"/><w:pPr><w:keepNext/><w:outlineLvl w:val="4"/></w:pPr><w:rPr><w:b/><w:sz w:val="22"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Heading6"><w:name w:val="heading 6"/><w:basedOn w:val="Normal"/><w:pPr><w:keepNext/><w:outlineLvl w:val="5"/></w:pPr><w:rPr><w:i/><w:sz w:val="22"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:pPr><w:ind w:left="567"/><w:pBdr><w:left w:val="single" w:sz="12" w:space="8" w:color="BFBFBF"/></w:pBdr></w:pPr><w:rPr><w:i/><w:color w:val="595959"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/><w:shd w:val="clear" w:color="auto" w:fill="F2F2F2"/></w:pPr><w:rPr><w:rFonts w:ascii="Consolas" w:hAnsi="Consolas" w:cs="Consolas"/><w:sz w:val="20"/></w:rPr></w:style>
<w:style w:type="paragraph" w:styleId="ListItem"><w:name w:val="List Item"/><w:basedOn w:val="Normal"/><w:pPr><w:spacing w:after="60"/><w:ind w:left="720" w:hanging="360"/></w:pPr></w:style>
</w:styles>"#;

/// The document as a Word (.docx) file.
pub fn to_docx(title: &str, blocks: &[Block]) -> Vec<u8> {
    let mut d = Doc { links: Vec::new() };
    let mut body = String::new();
    if !title.trim().is_empty() {
        d.para(Some("Title"), None, &[Inline::Text(title.to_string())], &mut body);
    }
    for b in blocks {
        match b {
            Block::Heading(n, x) => d.para(Some(&format!("Heading{n}")), None, x, &mut body),
            Block::Para(x) => d.para(None, None, x, &mut body),
            Block::Quote(x) => d.para(Some("Quote"), None, x, &mut body),
            Block::Code(t) => {
                for line in t.split('\n') {
                    d.para(Some("Code"), None, &[Inline::Text(line.to_string())], &mut body);
                }
            }
            Block::Rule => body.push_str("<w:p><w:pPr><w:pBdr><w:bottom w:val=\"single\" w:sz=\"6\" w:space=\"1\" w:color=\"BFBFBF\"/></w:pBdr></w:pPr></w:p>"),
            Block::Bullets(items) => {
                for it in items {
                    d.para(Some("ListItem"), Some("•\t"), it, &mut body);
                }
            }
            Block::Numbers(items) => {
                for (n, it) in items.iter().enumerate() {
                    d.para(Some("ListItem"), Some(&format!("{}.\t", n + 1)), it, &mut body);
                }
            }
        }
    }
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><w:body>{body}<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/><w:pgMar w:top=\"1417\" w:right=\"1417\" w:bottom=\"1134\" w:left=\"1417\" w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>"
    );
    let mut rels = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rIdStyles\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>");
    for (i, url) in d.links.iter().enumerate() {
        rels.push_str(&format!(
            "<Relationship Id=\"rIdL{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink\" Target=\"{}\" TargetMode=\"External\"/>",
            i + 1,
            xml(url)
        ));
    }
    rels.push_str("</Relationships>");
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/><Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/></Types>";
    let root_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>";
    zip_stored(&[
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", root_rels.as_bytes()),
        ("word/document.xml", document.as_bytes()),
        ("word/styles.xml", STYLES.as_bytes()),
        ("word/_rels/document.xml.rels", rels.as_bytes()),
    ])
}

// ------------------------------------------------------------------- zip

/// CRC-32 (IEEE), as used by zip.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A zip archive whose entries are stored uncompressed.
pub fn zip_stored(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    // 1980-01-01 00:00, the earliest DOS timestamp.
    let (time, date) = (0u16, 0x21u16);
    for (name, data) in files {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let len = data.len() as u32;
        let name_b = name.as_bytes();
        let mut local = Vec::new();
        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes()); // version needed
        local.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        local.extend_from_slice(&0u16.to_le_bytes()); // stored
        local.extend_from_slice(&time.to_le_bytes());
        local.extend_from_slice(&date.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&len.to_le_bytes());
        local.extend_from_slice(&len.to_le_bytes());
        local.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_b);
        out.extend_from_slice(&local);
        out.extend_from_slice(data);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&time.to_le_bytes());
        central.extend_from_slice(&date.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0u8; 8]); // extra, comment, disk, internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_b);
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0u8; 4]); // disk numbers
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&(files.len() as u16).to_le_bytes());
    out.extend_from_slice(&cd_len.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_and_inline() {
        let doc = "# Plan\n\nIntro with **bold**, *it*, `code` and [site](https://x.org).\nSecond line.\n\n- one\n- two\n\n1. first\n2. second\n\n> quoted\n\n```\nlet x = 1;\n```\n\n---\nsnake_case_name stays";
        let b = parse(doc);
        assert_eq!(b[0], Block::Heading(1, vec![Inline::Text("Plan".into())]));
        assert!(matches!(&b[1], Block::Para(v) if v.contains(&Inline::Break) && v.iter().any(|i| matches!(i, Inline::Link(_, u) if u == "https://x.org"))));
        assert!(matches!(&b[2], Block::Bullets(v) if v.len() == 2));
        let tasks = parse("- [x] done\n- [ ] open\n- plain");
        assert!(matches!(&tasks[0], Block::Bullets(v) if v[0][0] == Inline::Check(true) && v[1][0] == Inline::Check(false) && v[2][0] == Inline::Text("plain".into())));
        assert!(to_html(&tasks).contains("<input type=\"checkbox\" checked disabled> done"));
        assert!(matches!(&b[3], Block::Numbers(v) if v.len() == 2));
        assert!(matches!(&b[4], Block::Quote(_)));
        assert_eq!(b[5], Block::Code("let x = 1;".into()));
        assert_eq!(b[6], Block::Rule);
        assert_eq!(b[7], Block::Para(vec![Inline::Text("snake_case_name stays".into())]));
    }

    #[test]
    fn html_is_escaped_and_links_are_restricted() {
        let html = to_html(&parse("<script>alert(1)</script> [x](javascript:alert(1)) **<b>**"));
        assert!(!html.contains("<script>") && !html.contains("<b>"));
        assert!(!html.contains("href=\"javascript"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn crc_and_zip_layout() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let z = zip_stored(&[("a.txt", b"hello"), ("b/c.txt", b"")]);
        // Local header, then the end-of-central-directory record with two entries.
        assert_eq!(&z[..4], &[0x50, 0x4b, 0x03, 0x04]);
        let eocd = z.len() - 22;
        assert_eq!(&z[eocd..eocd + 4], &[0x50, 0x4b, 0x05, 0x06]);
        assert_eq!(u16::from_le_bytes([z[eocd + 10], z[eocd + 11]]), 2);
    }

    #[test]
    fn docx_contains_the_text_and_links() {
        let z = to_docx("Plan & Co", &parse("## Goals\n\nShip [it](https://x.org)."));
        let s = String::from_utf8_lossy(&z);
        assert!(s.contains("Plan &amp; Co"));
        assert!(s.contains("Heading2"));
        assert!(s.contains("TargetMode=\"External\""));
    }
}
