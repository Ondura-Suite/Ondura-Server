//! Contacts beyond the item store: vCard export/import (RFC 6350, written as
//! vCard 3.0, which every address book reads) and contact groups — named lists
//! of addresses that expand to their members when used as a recipient.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::db::{params, OptionalExtension};
use crate::model::{Address, Contact};
use crate::store::Store;

// ------------------------------------------------------------------ vCard

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

fn unesc(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split a structured value at unescaped `sep`.
fn split_unescaped(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            cur.push(c);
            if let Some(n) = it.next() {
                cur.push(n);
            }
        } else if c == sep {
            parts.push(unesc(&cur));
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    parts.push(unesc(&cur));
    parts
}

/// Fold a content line at 75 octets (on character boundaries).
fn fold(line: &str, out: &mut String) {
    let mut len = 0;
    for c in line.chars() {
        let n = c.len_utf8();
        if len + n > 75 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(c);
        len += n;
    }
    out.push_str("\r\n");
}

fn adr(a: &Address) -> Option<String> {
    let empty = a.street.is_empty() && a.city.is_empty() && a.state.is_empty() && a.postal_code.is_empty() && a.country.is_empty();
    (!empty).then(|| format!(";;{};{};{};{};{}", esc(&a.street), esc(&a.city), esc(&a.state), esc(&a.postal_code), esc(&a.country)))
}

/// One contact as a vCard.
pub fn to_vcard(c: &Contact, uid: Option<&str>) -> String {
    let mut lines: Vec<String> = vec!["BEGIN:VCARD".into(), "VERSION:3.0".into()];
    if let Some(u) = uid {
        lines.push(format!("UID:{}", esc(u)));
    }
    lines.push(format!("FN:{}", esc(&c.effective_name())));
    lines.push(format!("N:{};{};{};{};{}", esc(&c.last_name), esc(&c.first_name), esc(&c.middle_name), esc(&c.title), esc(&c.suffix)));
    let mut push = |name: &str, v: &str| {
        if !v.is_empty() {
            lines.push(format!("{name}:{}", esc(v)));
        }
    };
    push("NICKNAME", &c.nickname);
    push("TITLE", &c.job_title);
    push("TEL;TYPE=CELL", &c.mobile_phone);
    push("TEL;TYPE=WORK,VOICE", &c.business_phone);
    push("TEL;TYPE=WORK,VOICE", &c.business_phone2);
    push("TEL;TYPE=HOME,VOICE", &c.home_phone);
    push("TEL;TYPE=HOME,VOICE", &c.home_phone2);
    push("TEL;TYPE=WORK,FAX", &c.business_fax);
    push("TEL;TYPE=HOME,FAX", &c.home_fax);
    push("TEL;TYPE=PAGER", &c.pager);
    push("URL", &c.web_page);
    push("NOTE", &c.notes);
    if let Some(b) = &c.birthday {
        push("BDAY", b);
    }
    if !c.company.is_empty() || !c.department.is_empty() {
        lines.push(format!("ORG:{};{}", esc(&c.company), esc(&c.department)));
    }
    for e in c.emails.iter().filter(|e| !e.is_empty()) {
        lines.push(format!("EMAIL;TYPE=INTERNET:{}", esc(e)));
    }
    for (kind, a) in [("WORK", &c.business_address), ("HOME", &c.home_address), ("OTHER", &c.other_address)] {
        if let Some(v) = adr(a) {
            lines.push(format!("ADR;TYPE={kind}:{v}"));
        }
    }
    if !c.categories.is_empty() {
        lines.push(format!("CATEGORIES:{}", c.categories.iter().map(|x| esc(x)).collect::<Vec<_>>().join(",")));
    }
    if let Some(p) = c.picture.as_ref().filter(|p| !p.is_empty()) {
        lines.push(format!("PHOTO;ENCODING=b;TYPE=JPEG:{p}"));
    }
    lines.push("END:VCARD".into());
    let mut out = String::new();
    for l in lines {
        fold(&l, &mut out);
    }
    out
}

fn qp_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'=' && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse every vCard (2.1, 3.0 or 4.0) in `text` into contacts.
pub fn parse_vcards(text: &str) -> Vec<Contact> {
    // Unfold continuation lines; vCard 2.1 quoted-printable soft breaks too.
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n').map(|l| l.trim_end_matches('\r')) {
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            lines.last_mut().unwrap().push_str(&raw[1..]);
        } else if lines.last().is_some_and(|l| l.to_ascii_uppercase().contains("QUOTED-PRINTABLE") && l.ends_with('=')) {
            let l = lines.last_mut().unwrap();
            l.pop();
            l.push_str(raw);
        } else {
            lines.push(raw.to_string());
        }
    }
    let mut out = Vec::new();
    let mut cur: Option<Contact> = None;
    for line in lines {
        let Some((head, value)) = line.split_once(':') else { continue };
        let mut parts = head.split(';');
        let name = parts.next().unwrap_or("").rsplit('.').next().unwrap_or("").to_ascii_uppercase();
        let params: Vec<String> = parts.map(|p| p.to_ascii_uppercase()).collect();
        let types: Vec<String> = params
            .iter()
            .flat_map(|p| match p.split_once('=') {
                Some(("TYPE", v)) => v.split(',').map(|x| x.trim_matches('"').to_string()).collect::<Vec<_>>(),
                Some(_) => Vec::new(),
                None => vec![p.clone()],
            })
            .collect();
        let has = |t: &str| types.iter().any(|x| x == t);
        let value = if params.iter().any(|p| p.contains("QUOTED-PRINTABLE")) { qp_decode(value) } else { value.to_string() };
        match name.as_str() {
            "BEGIN" if value.eq_ignore_ascii_case("VCARD") => cur = Some(Contact::default()),
            "END" if value.eq_ignore_ascii_case("VCARD") => {
                if let Some(mut c) = cur.take() {
                    if c.display_name.is_empty() {
                        c.display_name = c.effective_name();
                    }
                    if !c.display_name.is_empty() || !c.emails.is_empty() {
                        out.push(c);
                    }
                }
            }
            _ => {
                let Some(c) = cur.as_mut() else { continue };
                match name.as_str() {
                    "FN" => c.display_name = unesc(&value),
                    "N" => {
                        let n = split_unescaped(&value, ';');
                        let get = |i: usize| n.get(i).cloned().unwrap_or_default();
                        (c.last_name, c.first_name, c.middle_name, c.title, c.suffix) = (get(0), get(1), get(2), get(3), get(4));
                    }
                    "NICKNAME" => c.nickname = unesc(&value),
                    "TITLE" => c.job_title = unesc(&value),
                    "ORG" => {
                        let o = split_unescaped(&value, ';');
                        c.company = o.first().cloned().unwrap_or_default();
                        c.department = o.get(1).cloned().unwrap_or_default();
                    }
                    "EMAIL" if c.emails.len() < 3 => c.emails.push(unesc(&value).trim().to_string()),
                    "TEL" => {
                        let v = unesc(&value).trim().trim_start_matches("tel:").to_string();
                        let slot = if has("FAX") {
                            if has("HOME") { &mut c.home_fax } else { &mut c.business_fax }
                        } else if has("CELL") {
                            &mut c.mobile_phone
                        } else if has("PAGER") {
                            &mut c.pager
                        } else if has("HOME") {
                            if c.home_phone.is_empty() { &mut c.home_phone } else { &mut c.home_phone2 }
                        } else if has("WORK") {
                            if c.business_phone.is_empty() { &mut c.business_phone } else { &mut c.business_phone2 }
                        } else if c.mobile_phone.is_empty() {
                            &mut c.mobile_phone
                        } else if c.business_phone.is_empty() {
                            &mut c.business_phone
                        } else {
                            &mut c.home_phone
                        };
                        if slot.is_empty() {
                            *slot = v;
                        }
                    }
                    "ADR" => {
                        let a = split_unescaped(&value, ';');
                        let get = |i: usize| a.get(i).cloned().unwrap_or_default();
                        let street = [get(1), get(2)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("\n");
                        let addr = Address { street, city: get(3), state: get(4), postal_code: get(5), country: get(6) };
                        if has("HOME") {
                            c.home_address = addr;
                        } else if has("WORK") {
                            c.business_address = addr;
                        } else {
                            c.other_address = addr;
                        }
                    }
                    "URL" => c.web_page = unesc(&value),
                    "NOTE" => c.notes = unesc(&value),
                    "BDAY" => {
                        let d = value.trim();
                        // 19800521 or 1980-05-21 (times dropped).
                        let d = d.split('T').next().unwrap_or("");
                        let iso = if d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()) { format!("{}-{}-{}", &d[..4], &d[4..6], &d[6..]) } else { d.to_string() };
                        if iso.len() == 10 {
                            c.birthday = Some(iso);
                        }
                    }
                    "CATEGORIES" => c.categories = split_unescaped(&value, ',').into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                    "PHOTO" => {
                        let v = value.trim();
                        if let Some(rest) = v.strip_prefix("data:") {
                            if let Some((_, b64)) = rest.split_once(";base64,") {
                                c.picture = Some(b64.to_string());
                            }
                        } else if params.iter().any(|p| p == "ENCODING=B" || p == "ENCODING=BASE64" || p == "BASE64") {
                            c.picture = Some(v.to_string());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------- contact groups

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMember {
    #[serde(default)]
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContactGroup {
    pub id: i64,
    pub name: String,
    pub members: Vec<GroupMember>,
}

impl Store {
    pub fn contact_groups(&self, user_id: i64) -> Result<Vec<ContactGroup>> {
        let db = self.db();
        let st = db.prepare("SELECT id, name, members FROM contact_groups WHERE user_id = ?1 ORDER BY LOWER(name), id")?;
        st.query_map([user_id], |r| {
            let m: String = r.get(2)?;
            Ok(ContactGroup { id: r.get(0)?, name: r.get(1)?, members: serde_json::from_str(&m).unwrap_or_default() })
        })
    }

    /// Create (`id` = `None`) or replace a group. Members need an address;
    /// duplicates (by address) are dropped.
    pub fn save_contact_group(&self, user_id: i64, id: Option<i64>, name: &str, members: &[GroupMember]) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            bail!("the group needs a name");
        }
        let mut clean: Vec<GroupMember> = Vec::new();
        for m in members {
            let email = m.email.trim();
            if !email.contains('@') || clean.iter().any(|x| x.email.eq_ignore_ascii_case(email)) {
                continue;
            }
            clean.push(GroupMember { name: m.name.trim().to_string(), email: email.to_string() });
        }
        let json = serde_json::to_string(&clean)?;
        let db = self.db();
        match id {
            Some(id) => {
                let n = db.execute("UPDATE contact_groups SET name = ?1, members = ?2 WHERE id = ?3 AND user_id = ?4", params![name, json, id, user_id])?;
                if n == 0 {
                    bail!("no such group");
                }
                Ok(id)
            }
            None => db.query_row("INSERT INTO contact_groups (user_id, name, members) VALUES (?1, ?2, ?3) RETURNING id", params![user_id, name, json], |r| r.get(0)),
        }
    }

    pub fn delete_contact_group(&self, user_id: i64, id: i64) -> Result<()> {
        self.db().execute("DELETE FROM contact_groups WHERE id = ?1 AND user_id = ?2", params![id, user_id])?;
        Ok(())
    }

    pub fn contact_group(&self, user_id: i64, id: i64) -> Result<Option<ContactGroup>> {
        self.db()
            .query_row("SELECT id, name, members FROM contact_groups WHERE id = ?1 AND user_id = ?2", params![id, user_id], |r| {
                let m: String = r.get(2)?;
                Ok(ContactGroup { id: r.get(0)?, name: r.get(1)?, members: serde_json::from_str(&m).unwrap_or_default() })
            })
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vcard_round_trip() {
        let c = Contact {
            first_name: "Anna".into(),
            last_name: "Becker".into(),
            display_name: "Anna Becker".into(),
            company: "Müller; Söhne".into(),
            department: "Einkauf".into(),
            job_title: "Leitung".into(),
            emails: vec!["anna@example.com".into(), "a.becker@example.org".into()],
            mobile_phone: "+49 170 1234567".into(),
            business_phone: "+49 6894 1234".into(),
            business_address: Address { street: "Bezirksstraße 1".into(), city: "Niederwürzbach".into(), postal_code: "66440".into(), country: "Deutschland".into(), ..Default::default() },
            birthday: Some("1985-04-12".into()),
            notes: "Line one\nLine, two; with \\ backslash and a very long text that certainly needs folding beyond seventy-five octets".into(),
            categories: vec!["Kunde".into(), "VIP".into()],
            ..Default::default()
        };
        let v = to_vcard(&c, Some("abc"));
        assert!(v.lines().all(|l| l.len() <= 76));
        let back = parse_vcards(&v);
        assert_eq!(back.len(), 1);
        let b = &back[0];
        assert_eq!(b.display_name, c.display_name);
        assert_eq!((b.first_name.as_str(), b.last_name.as_str()), ("Anna", "Becker"));
        assert_eq!(b.company, c.company);
        assert_eq!(b.department, c.department);
        assert_eq!(b.emails, c.emails);
        assert_eq!(b.mobile_phone, c.mobile_phone);
        assert_eq!(b.business_phone, c.business_phone);
        assert_eq!(b.business_address, c.business_address);
        assert_eq!(b.birthday, c.birthday);
        assert_eq!(b.notes, c.notes);
        assert_eq!(b.categories, c.categories);
    }

    #[test]
    fn reads_other_dialects() {
        let v21 = "BEGIN:VCARD\r\nVERSION:2.1\r\nN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:M=C3=BCller;J=C3=BCrgen\r\nTEL;CELL:0170 1\r\nTEL;HOME;VOICE:06894 2\r\nEMAIL;PREF;INTERNET:j@example.com\r\nEND:VCARD\r\n";
        let v4 = "BEGIN:VCARD\nVERSION:4.0\nFN:Bob\nitem1.EMAIL;TYPE=work:bob@example.com\nTEL;VALUE=uri;TYPE=\"voice,work\":tel:+49-1\nBDAY:19800521\nPHOTO:data:image/jpeg;base64,AAAA\nEND:VCARD\n";
        let all = parse_vcards(&format!("{v21}{v4}"));
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].display_name, "Jürgen Müller");
        assert_eq!(all[0].mobile_phone, "0170 1");
        assert_eq!(all[0].home_phone, "06894 2");
        assert_eq!(all[0].emails, vec!["j@example.com".to_string()]);
        assert_eq!(all[1].emails, vec!["bob@example.com".to_string()]);
        assert_eq!(all[1].business_phone, "+49-1");
        assert_eq!(all[1].birthday.as_deref(), Some("1980-05-21"));
        assert_eq!(all[1].picture.as_deref(), Some("AAAA"));
    }

    #[test]
    fn groups_keep_unique_addresses() {
        let (app, user) = crate::app::App::test();
        let s = &app.store;
        let m = |n: &str, e: &str| GroupMember { name: n.into(), email: e.into() };
        let id = s.save_contact_group(user.id, None, "Team", &[m("A", "a@x.de"), m("", "A@X.de"), m("", "nope"), m("B", "b@x.de")]).unwrap();
        let g = s.contact_group(user.id, id).unwrap().unwrap();
        assert_eq!(g.members.iter().map(|x| x.email.as_str()).collect::<Vec<_>>(), ["a@x.de", "b@x.de"]);
        let other = s.create_user("o@example.com", "O", "pw", "user").unwrap();
        assert!(s.save_contact_group(other.id, Some(id), "Mine", &[]).is_err());
        assert!(s.contact_group(other.id, id).unwrap().is_none());
        s.save_contact_group(user.id, Some(id), "Team 2", &[]).unwrap();
        assert_eq!(s.contact_groups(user.id).unwrap()[0].name, "Team 2");
        s.delete_contact_group(user.id, id).unwrap();
        assert!(s.contact_groups(user.id).unwrap().is_empty());
    }
}
