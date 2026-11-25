//! Reading and writing RFC 5322 / MIME messages.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use mail_parser::{Address as MpAddress, HeaderValue, MessageParser, MimeHeaders, PartType};

use crate::model::{Importance, Person};

/// Index data of a message, stored next to the raw MIME.
#[derive(Debug, Clone, Default)]
pub struct MailMeta {
    pub subject: String,
    pub from: Option<Person>,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    pub bcc: Vec<Person>,
    pub reply_to: Vec<Person>,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    /// Space-joined `References` message-ids, oldest first (threading).
    pub references: Option<String>,
    pub importance: Importance,
    pub preview: String,
    pub has_attachments: bool,
    /// Exchange message class (`IPM.Note`, `IPM.Schedule.Meeting.Request`, …).
    pub message_class: String,
    /// iCalendar data of a meeting message.
    pub calendar: Option<String>,
}

fn persons(addr: Option<&MpAddress<'_>>) -> Vec<Person> {
    let Some(addr) = addr else { return vec![] };
    addr.iter()
        .filter_map(|a| {
            let email = a.address()?.trim().to_string();
            if email.is_empty() {
                return None;
            }
            Some(Person { name: a.name().map(|n| n.trim().to_string()).unwrap_or_default(), email })
        })
        .collect()
}

fn text_list(v: &HeaderValue<'_>) -> Vec<String> {
    match v {
        HeaderValue::Text(t) => vec![t.to_string()],
        HeaderValue::TextList(l) => l.iter().map(|s| s.to_string()).collect(),
        _ => vec![],
    }
}

pub fn parse_meta(raw: &[u8]) -> MailMeta {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return MailMeta { message_class: "IPM.Note".into(), ..Default::default() };
    };
    let importance = match msg
        .header_raw("Importance")
        .or_else(|| msg.header_raw("X-Priority"))
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some(v) if v.starts_with("high") || v.starts_with('1') || v.starts_with('2') => Importance::High,
        Some(v) if v.starts_with("low") || v.starts_with('4') || v.starts_with('5') => Importance::Low,
        _ => Importance::Normal,
    };
    let calendar = msg
        .parts
        .iter()
        .find(|p| p.is_content_type("text", "calendar"))
        .and_then(|p| match &p.body {
            PartType::Text(t) => Some(t.to_string()),
            PartType::Binary(b) | PartType::InlineBinary(b) => Some(String::from_utf8_lossy(b).into_owned()),
            _ => None,
        });
    let message_class = match &calendar {
        Some(ics) => {
            let method = ics
                .lines()
                .find_map(|l| l.strip_prefix("METHOD:"))
                .map(|m| m.trim().to_ascii_uppercase())
                .unwrap_or_default();
            match method.as_str() {
                "REQUEST" => "IPM.Schedule.Meeting.Request".to_string(),
                "CANCEL" => "IPM.Schedule.Meeting.Canceled".to_string(),
                "REPLY" => {
                    let partstat = ics.lines().find_map(|l| l.split(";PARTSTAT=").nth(1)).map(|s| s.split([';', ':']).next().unwrap_or("").to_ascii_uppercase());
                    match partstat.as_deref() {
                        Some("ACCEPTED") => "IPM.Schedule.Meeting.Resp.Pos".into(),
                        Some("DECLINED") => "IPM.Schedule.Meeting.Resp.Neg".into(),
                        _ => "IPM.Schedule.Meeting.Resp.Tent".into(),
                    }
                }
                _ => "IPM.Note".into(),
            }
        }
        None if msg.is_content_type("multipart", "signed") => "IPM.Note.SMIME.MultipartSigned".into(),
        None if msg.content_type().is_some_and(|c| c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("pkcs7-mime"))) => "IPM.Note.SMIME".into(),
        None => "IPM.Note".into(),
    };
    let preview = preview_of(&msg);
    let has_attachments = msg.attachments().any(|a| !a.is_content_type("text", "calendar") && !is_inline_image(a));
    MailMeta {
        subject: msg.subject().unwrap_or_default().trim().to_string(),
        from: persons(msg.from()).into_iter().next(),
        to: persons(msg.to()),
        cc: persons(msg.cc()),
        bcc: persons(msg.bcc()),
        reply_to: persons(msg.reply_to()),
        date: msg.date().map(|d| d.to_timestamp()),
        message_id: msg.message_id().map(|m| format!("<{}>", m.trim_matches(['<', '>']))),
        in_reply_to: text_list(msg.in_reply_to()).into_iter().next().map(|m| format!("<{}>", m.trim_matches(['<', '>']))),
        references: {
            let refs: Vec<String> = text_list(msg.references()).into_iter().map(|m| format!("<{}>", m.trim_matches(['<', '>']))).collect();
            if refs.is_empty() { None } else { Some(refs.join(" ")) }
        },
        importance,
        preview,
        has_attachments,
        message_class,
        calendar,
    }
}

fn is_inline_image(p: &mail_parser::MessagePart<'_>) -> bool {
    p.content_id().is_some() && p.content_disposition().is_none_or(|d| d.ctype().eq_ignore_ascii_case("inline")) && p.content_type().is_some_and(|c| c.ctype().eq_ignore_ascii_case("image"))
}


fn preview_of(msg: &mail_parser::Message<'_>) -> String {
    let text = msg
        .body_text(0)
        .map(|t| t.into_owned())
        .or_else(|| msg.body_html(0).map(|h| html_to_text(&h)))
        .unwrap_or_default();
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(255).collect()
}

/// Bodies of a message: (plain text, HTML).
pub fn bodies(raw: &[u8]) -> (Option<String>, Option<String>) {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return (Some(String::from_utf8_lossy(raw).into_owned()), None);
    };
    let html = msg.html_bodies().next().and_then(|p| match &p.body {
        PartType::Html(h) => Some(h.to_string()),
        _ => None,
    });
    let text = msg.text_bodies().next().and_then(|p| match &p.body {
        PartType::Text(t) => Some(t.to_string()),
        _ => None,
    });
    let text = text.or_else(|| html.as_deref().map(html_to_text));
    (text, html)
}

#[derive(Debug, Clone)]
pub struct AttachmentInfo {
    /// Index among the attachments of the message (stable for a given MIME).
    pub index: usize,
    pub name: String,
    pub content_type: String,
    pub size: usize,
    pub content_id: Option<String>,
    pub inline: bool,
}

pub fn attachments(raw: &[u8]) -> Vec<AttachmentInfo> {
    let Some(msg) = MessageParser::default().parse(raw) else { return vec![] };
    msg.attachments()
        .enumerate()
        .filter(|(_, p)| !p.is_content_type("text", "calendar"))
        .map(|(index, p)| {
            let content_type = p
                .content_type()
                .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
                .unwrap_or_else(|| "application/octet-stream".into());
            AttachmentInfo {
                index,
                name: p.attachment_name().map(str::to_string).unwrap_or_else(|| format!("attachment-{}", index + 1)),
                content_type,
                size: p.contents().len(),
                content_id: p.content_id().map(|c| c.trim_matches(['<', '>']).to_string()),
                inline: is_inline_image(p),
            }
        })
        .collect()
}

pub fn attachment_data(raw: &[u8], index: usize) -> Option<(AttachmentInfo, Vec<u8>)> {
    let info = attachments(raw).into_iter().find(|a| a.index == index)?;
    let msg = MessageParser::default().parse(raw)?;
    let part = msg.attachments().nth(index)?;
    let data = match &part.body {
        PartType::Message(m) => m.raw_message.to_vec(),
        _ => part.contents().to_vec(),
    };
    Some((info, data))
}

/// Very small HTML → text conversion for previews and plain-text bodies.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut in_tag = false;
    let mut tag = String::new();
    let mut skip_depth = 0usize;
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if in_tag {
            if c == '>' {
                in_tag = false;
                let t = tag.trim().to_ascii_lowercase();
                let name: String = t.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                let closing = t.starts_with('/');
                if name == "style" || name == "script" || name == "head" {
                    if closing {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if !t.ends_with('/') {
                        skip_depth += 1;
                    }
                }
                if matches!(name.as_str(), "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "table") && !out.ends_with('\n') {
                    out.push('\n');
                }
                tag.clear();
            } else {
                tag.push(c);
            }
            continue;
        }
        if c == '<' {
            in_tag = true;
            continue;
        }
        if skip_depth > 0 {
            continue;
        }
        if c == '&' {
            let mut entity = String::new();
            while let Some(&n) = chars.peek() {
                if n == ';' || entity.len() > 8 {
                    break;
                }
                entity.push(n);
                chars.next();
            }
            if chars.peek() == Some(&';') {
                chars.next();
            }
            let decoded: String = match entity.as_str() {
                "amp" => "&".into(),
                "lt" => "<".into(),
                "gt" => ">".into(),
                "quot" => "\"".into(),
                "apos" | "#39" => "'".into(),
                "nbsp" | "#160" => " ".into(),
                _ => entity
                    .strip_prefix('#')
                    .and_then(|num| num.strip_prefix('x').map_or_else(|| num.parse().ok(), |h| u32::from_str_radix(h, 16).ok()))
                    .and_then(char::from_u32)
                    .map(String::from)
                    .unwrap_or_default(),
            };
            out.push_str(&decoded);
            continue;
        }
        out.push(c);
    }
    let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    let mut s = lines.join("\n");
    while s.contains("\n\n\n") {
        s = s.replace("\n\n\n", "\n\n");
    }
    s.trim().to_string()
}

pub fn text_to_html(text: &str) -> String {
    let escaped = text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<html><body><div style=\"font-family:sans-serif\">{}</div></body></html>", escaped.replace('\n', "<br>\n"))
}

// ----------------------------------------------------------------- writing

/// RFC 2047 encoding of a header value when it is not plain ASCII.
pub fn encode_word(s: &str) -> String {
    if s.is_ascii() && !s.contains(['\r', '\n']) {
        return s.to_string();
    }
    format!("=?UTF-8?B?{}?=", B64.encode(s.as_bytes()))
}

pub fn format_person(p: &Person) -> String {
    if p.name.is_empty() {
        format!("<{}>", p.email)
    } else if p.name.is_ascii() && !p.name.contains(['"', ',', ';', '<', '>', '@', '(', ')']) {
        format!("{} <{}>", p.name, p.email)
    } else if p.name.is_ascii() {
        format!("\"{}\" <{}>", p.name.replace('\\', "\\\\").replace('"', "\\\""), p.email)
    } else {
        format!("{} <{}>", encode_word(&p.name), p.email)
    }
}

pub fn format_list(list: &[Person]) -> String {
    list.iter().map(format_person).collect::<Vec<_>>().join(", ")
}

pub fn new_message_id(hostname: &str) -> String {
    format!("<{}@{}>", uuid::Uuid::new_v4().simple(), hostname)
}

pub fn rfc2822_now() -> String {
    Utc::now().to_rfc2822()
}

fn wrap_base64(data: &[u8]) -> String {
    let enc = B64.encode(data);
    let mut out = String::with_capacity(enc.len() + enc.len() / 76 * 2);
    for chunk in enc.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push_str("\r\n");
    }
    out
}

/// A message built by the server (meeting invitations, replies, bounces, …).
#[derive(Debug, Clone, Default)]
pub struct Outgoing {
    pub from: Person,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    /// (method, iCalendar body)
    pub calendar: Option<(String, String)>,
    /// (content type, raw bytes) attached as a whole message/file.
    pub attachment: Option<(String, String, Vec<u8>)>,
    pub headers: Vec<(String, String)>,
    pub in_reply_to: Option<String>,
    pub multipart_report: Option<String>,
    /// Further attachments.
    pub files: Vec<File>,
}

#[derive(Debug, Clone, Default)]
pub struct File {
    pub name: String,
    pub content_type: String,
    pub data: Vec<u8>,
    pub content_id: Option<String>,
    pub inline: bool,
}

impl File {
    fn part(&self) -> String {
        let cid = self.content_id.as_ref().map(|c| format!("Content-ID: <{c}>\r\n")).unwrap_or_default();
        format!(
            "Content-Type: {}; name=\"{}\"\r\nContent-Disposition: {}; filename=\"{}\"\r\n{cid}Content-Transfer-Encoding: base64\r\n\r\n{}",
            self.content_type,
            encode_word(&self.name),
            if self.inline { "inline" } else { "attachment" },
            encode_word(&self.name),
            wrap_base64(&self.data)
        )
    }
}

impl Outgoing {
    pub fn build(&self, hostname: &str) -> Vec<u8> {
        let mut h = String::new();
        h.push_str(&format!("From: {}\r\n", format_person(&self.from)));
        if !self.to.is_empty() {
            h.push_str(&format!("To: {}\r\n", format_list(&self.to)));
        }
        if !self.cc.is_empty() {
            h.push_str(&format!("Cc: {}\r\n", format_list(&self.cc)));
        }
        h.push_str(&format!("Subject: {}\r\n", encode_word(&self.subject)));
        h.push_str(&format!("Date: {}\r\n", rfc2822_now()));
        h.push_str(&format!("Message-ID: {}\r\n", new_message_id(hostname)));
        if let Some(r) = &self.in_reply_to {
            h.push_str(&format!("In-Reply-To: {r}\r\nReferences: {r}\r\n"));
        }
        for (k, v) in &self.headers {
            h.push_str(&format!("{k}: {v}\r\n"));
        }
        h.push_str("MIME-Version: 1.0\r\n");

        let text_part = format!("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}", wrap_base64(self.text.as_bytes()));
        let mut alternatives = vec![text_part];
        if let Some(html) = &self.html {
            alternatives.push(format!("Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}", wrap_base64(html.as_bytes())));
        }
        if let Some((method, ics)) = &self.calendar {
            alternatives.push(format!(
                "Content-Type: text/calendar; charset=utf-8; method={method}\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
                wrap_base64(ics.as_bytes())
            ));
        }
        let body = if alternatives.len() == 1 {
            alternatives.remove(0)
        } else {
            let b = format!("alt-{}", uuid::Uuid::new_v4().simple());
            let mut s = format!("Content-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n");
            for part in &alternatives {
                s.push_str(&format!("--{b}\r\n{part}"));
            }
            s.push_str(&format!("--{b}--\r\n"));
            s
        };
        let mut extra: Vec<String> = Vec::new();
        if let Some((ctype, name, data)) = &self.attachment {
            extra.push(if ctype.starts_with("message/") || ctype.starts_with("text/rfc822") {
                format!("Content-Type: {ctype}\r\nContent-Disposition: inline\r\n\r\n{}\r\n", String::from_utf8_lossy(data))
            } else {
                File { name: name.clone(), content_type: ctype.clone(), data: data.clone(), content_id: None, inline: false }.part()
            });
        }
        extra.extend(self.files.iter().map(File::part));
        let body = if extra.is_empty() {
            body
        } else {
            let b = format!("mix-{}", uuid::Uuid::new_v4().simple());
            let outer = match &self.multipart_report {
                Some(r) => format!("multipart/report; report-type={r}"),
                None => "multipart/mixed".to_string(),
            };
            let mut s = format!("Content-Type: {outer}; boundary=\"{b}\"\r\n\r\n--{b}\r\n{body}");
            for p in extra {
                s.push_str(&format!("--{b}\r\n{p}"));
            }
            s.push_str(&format!("--{b}--\r\n"));
            s
        };
        let mut out = h.into_bytes();
        out.extend_from_slice(body.as_bytes());
        out
    }
}

/// Attachments of a message as files (to rebuild it).
pub fn files_of(raw: &[u8]) -> Vec<File> {
    attachments(raw)
        .into_iter()
        .filter_map(|a| attachment_data(raw, a.index))
        .map(|(info, data)| File { name: info.name, content_type: info.content_type, data, content_id: info.content_id, inline: info.inline })
        .collect()
}

/// The same message with a different set of attachments.
pub fn with_files(raw: &[u8], files: Vec<File>, hostname: &str) -> Vec<u8> {
    let meta = parse_meta(raw);
    let (text, html) = bodies(raw);
    let mut headers = Vec::new();
    if !meta.bcc.is_empty() {
        headers.push(("Bcc".to_string(), format_list(&meta.bcc)));
    }
    Outgoing {
        from: meta.from.unwrap_or_default(),
        to: meta.to,
        cc: meta.cc,
        subject: meta.subject,
        text: text.unwrap_or_default(),
        html,
        headers,
        files,
        in_reply_to: meta.in_reply_to,
        ..Default::default()
    }
    .build(hostname)
}

/// Remove the Bcc header from a message before it leaves the server.
pub fn strip_bcc(raw: &[u8]) -> Vec<u8> {
    let header_end = find_header_end(raw);
    let (head, body) = raw.split_at(header_end);
    let head = String::from_utf8_lossy(head);
    let mut out = String::with_capacity(head.len());
    let mut skipping = false;
    for line in head.split_inclusive('\n') {
        let folded = line.starts_with([' ', '\t']);
        if !folded {
            skipping = line.len() >= 4 && line[..4].eq_ignore_ascii_case("bcc:");
        }
        if !skipping {
            out.push_str(line);
        }
    }
    let mut v = out.into_bytes();
    v.extend_from_slice(body);
    v
}

/// Offset just past the blank line ending the header (or the end).
pub fn find_header_end(raw: &[u8]) -> usize {
    raw.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|p| p + 2))
        .unwrap_or(raw.len())
}

/// Whether the header section contains `name`.
pub fn has_header(raw: &[u8], name: &str) -> bool {
    let head = String::from_utf8_lossy(&raw[..find_header_end(raw)]);
    let prefix = format!("{}:", name.to_ascii_lowercase());
    head.lines().any(|l| l.to_ascii_lowercase().starts_with(&prefix))
}

/// Prepend header lines to a message.
pub fn prepend_headers(raw: &[u8], headers: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 256);
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(raw);
    out
}

/// Fill in Date and Message-ID when a client left them out.
pub fn complete_headers(raw: &[u8], hostname: &str) -> Vec<u8> {
    let mut add = Vec::new();
    if !has_header(raw, "Date") {
        add.push(("Date".to_string(), rfc2822_now()));
    }
    if !has_header(raw, "Message-ID") {
        add.push(("Message-ID".to_string(), new_message_id(hostname)));
    }
    if !has_header(raw, "MIME-Version") {
        add.push(("MIME-Version".to_string(), "1.0".to_string()));
    }
    if add.is_empty() { raw.to_vec() } else { prepend_headers(raw, &add) }
}

/// Normalise line endings to CRLF (SMTP requires it, some clients send LF).
pub fn crlf(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 40);
    let mut prev = 0u8;
    for &b in raw {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

/// All recipient addresses (To, Cc, Bcc) of a message.
pub fn recipients(raw: &[u8]) -> Vec<String> {
    let meta = parse_meta(raw);
    let mut out: Vec<String> = Vec::new();
    for p in meta.to.iter().chain(&meta.cc).chain(&meta.bcc) {
        let e = p.email.to_ascii_lowercase();
        if e.contains('@') && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// Wrap the original message for a reply/forward created by the server
/// (ActiveSync SmartReply/SmartForward without ReplaceMime): the client's
/// new message plus the original as attachment (forward) or quoted text (reply).
pub fn combine_with_original(new_raw: &[u8], original: &[u8], forward: bool) -> Vec<u8> {
    let header_end = find_header_end(new_raw);
    let head = &new_raw[..header_end];
    // Keep only the top-level headers that are not about content.
    let head_str = String::from_utf8_lossy(head);
    let mut kept = String::new();
    let mut skipping = false;
    for line in head_str.split_inclusive('\n') {
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            let l = line.to_ascii_lowercase();
            skipping = l.starts_with("content-type:") || l.starts_with("content-transfer-encoding:") || l.starts_with("mime-version:");
        }
        if !skipping {
            kept.push_str(line);
        }
    }
    let (new_text, new_html) = bodies(new_raw);
    let (orig_text, orig_html) = bodies(original);
    let orig_meta = parse_meta(original);
    let header_block = format!(
        "From: {}\nSent: {}\nTo: {}\nSubject: {}",
        orig_meta.from.as_ref().map(format_person).unwrap_or_default(),
        orig_meta.date.and_then(|d| chrono::DateTime::from_timestamp(d, 0)).map(|d| d.to_rfc2822()).unwrap_or_default(),
        format_list(&orig_meta.to),
        orig_meta.subject
    );
    let text = format!("{}\n\n-----Original Message-----\n{}\n\n{}", new_text.unwrap_or_default(), header_block, orig_text.unwrap_or_default());
    let html = format!(
        "{}<hr><div>{}</div><br>{}",
        new_html.unwrap_or_else(|| text_to_html(&text_before_original(&text))),
        header_block.replace('&', "&amp;").replace('<', "&lt;").replace('\n', "<br>"),
        orig_html.unwrap_or_else(|| text_to_html(&orig_text_of(original)))
    );
    let b = format!("alt-{}", uuid::Uuid::new_v4().simple());
    let alt = format!(
        "Content-Type: multipart/alternative; boundary=\"{b}\"\r\n\r\n--{b}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}--{b}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}--{b}--\r\n",
        wrap_base64(text.as_bytes()),
        wrap_base64(html.as_bytes())
    );
    // Attachments of the client's own message and, for forwards, of the original.
    let mut parts = vec![alt];
    for source in if forward { vec![new_raw, original] } else { vec![new_raw] } {
        for a in attachments(source) {
            if let Some((info, data)) = attachment_data(source, a.index) {
                let cid = info.content_id.as_ref().map(|c| format!("Content-ID: <{c}>\r\n")).unwrap_or_default();
                parts.push(format!(
                    "Content-Type: {}; name=\"{}\"\r\nContent-Disposition: {}; filename=\"{}\"\r\n{cid}Content-Transfer-Encoding: base64\r\n\r\n{}",
                    info.content_type,
                    encode_word(&info.name),
                    if info.inline { "inline" } else { "attachment" },
                    encode_word(&info.name),
                    wrap_base64(&data)
                ));
            }
        }
    }
    let body = if parts.len() == 1 {
        parts.remove(0)
    } else {
        let m = format!("mix-{}", uuid::Uuid::new_v4().simple());
        let mut s = format!("Content-Type: multipart/mixed; boundary=\"{m}\"\r\n\r\n");
        for p in &parts {
            s.push_str(&format!("--{m}\r\n{p}"));
        }
        s.push_str(&format!("--{m}--\r\n"));
        s
    };
    let mut out = kept.replace("\r\n", "\n").replace('\n', "\r\n").into_bytes();
    out.extend_from_slice(b"MIME-Version: 1.0\r\n");
    out.extend_from_slice(body.as_bytes());
    out
}

fn text_before_original(text: &str) -> String {
    text.split("\n\n-----Original Message-----").next().unwrap_or_default().to_string()
}

fn orig_text_of(raw: &[u8]) -> String {
    bodies(raw).0.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"From: Max Becker <max@example.com>\r\nTo: Niklas <niklas@mirranet.de>, anna@mirranet.de\r\nBcc: secret@example.com\r\nSubject: =?UTF-8?B?w5xiZXJzaWNodA==?=\r\nDate: Sun, 22 Nov 2026 18:00:00 +0100\r\nMessage-ID: <abc@example.com>\r\nImportance: high\r\nContent-Type: multipart/mixed; boundary=X\r\n\r\n--X\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hallo &amp; willkommen</p><style>p{}</style>\r\n--X\r\nContent-Type: application/pdf; name=a.pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERg==\r\n--X--\r\n";

    #[test]
    fn meta_of_sample() {
        let m = parse_meta(SAMPLE);
        assert_eq!(m.subject, "Übersicht");
        assert_eq!(m.from.unwrap().email, "max@example.com");
        assert_eq!(m.to.len(), 2);
        assert_eq!(m.importance, Importance::High);
        assert!(m.has_attachments);
        assert_eq!(m.preview, "Hallo & willkommen");
        assert_eq!(m.message_id.as_deref(), Some("<abc@example.com>"));
        assert_eq!(recipients(SAMPLE), vec!["niklas@mirranet.de", "anna@mirranet.de", "secret@example.com"]);
        let a = attachments(SAMPLE);
        assert_eq!(a[0].name, "a.pdf");
        assert_eq!(attachment_data(SAMPLE, 0).unwrap().1, b"%PDF");
    }

    #[test]
    fn strips_bcc() {
        let s = String::from_utf8(strip_bcc(SAMPLE)).unwrap();
        assert!(!s.contains("secret@"));
        assert!(s.contains("Subject:"));
    }

    #[test]
    fn builds_messages() {
        let o = Outgoing {
            from: Person::new("Niklas Linz", "niklas@mirranet.de"),
            to: vec![Person::new("", "max@example.com")],
            subject: "Einladung: Jour fixe".into(),
            text: "Hallo".into(),
            calendar: Some(("REQUEST".into(), "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n".into())),
            ..Default::default()
        };
        let raw = o.build("mail.mirranet.de");
        let m = parse_meta(&raw);
        assert_eq!(m.subject, "Einladung: Jour fixe");
        assert_eq!(m.message_class, "IPM.Schedule.Meeting.Request");
        assert!(m.calendar.unwrap().contains("METHOD:REQUEST"));
    }

    #[test]
    fn html_conversion() {
        assert_eq!(html_to_text("<html><head><title>x</title></head><body>A<br>B&nbsp;&#x41;</body></html>"), "A\nB A");
    }
}
