//! Conversion between stored items and ActiveSync `ApplicationData`
//! (MS-ASEMAIL, MS-ASCAL, MS-ASCNTC, MS-ASTASK, MS-ASNOTE).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use ondura_wbxml::Element;

use crate::mime;
use crate::model::{
    self, Address, Attendee, AttendeeKind, BusyStatus, Contact, Event, Exception, Frequency, Importance, Note, Person, Recurrence, Response, Sensitivity, Task,
    flags,
};
use crate::store::Item;
use crate::tzinfo;

// ---------------------------------------------------------------- threading

/// A stable per-conversation key for an item: the thread root message-id from
/// References, else In-Reply-To, else its own Message-ID, else the normalized
/// subject. Hashed into the ActiveSync ConversationId.
fn thread_key(item: &Item) -> String {
    if let Some(root) = item.references.as_deref().and_then(|r| r.split_whitespace().next()) {
        return root.to_ascii_lowercase();
    }
    if let Some(parent) = item.in_reply_to.as_deref().filter(|s| !s.is_empty()) {
        return parent.to_ascii_lowercase();
    }
    if let Some(mid) = item.message_id.as_deref().filter(|s| !s.is_empty()) {
        return mid.to_ascii_lowercase();
    }
    let mut s = item.subject.trim().to_ascii_lowercase();
    loop {
        let t = s.trim_start();
        let stripped = ["re:", "aw:", "fwd:", "wg:", "fw:"].iter().find_map(|p| t.strip_prefix(p));
        match stripped {
            Some(rest) => s = rest.trim_start().to_string(),
            None => break,
        }
    }
    s
}

// ------------------------------------------------------------------ dates

pub fn iso(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%S.000Z").to_string()
}

pub fn compact(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Parse both the compact (`20261122T090000Z`) and the ISO form.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp());
    }
    let z = s.trim_end_matches('Z');
    for fmt in ["%Y%m%dT%H%M%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(z, fmt) {
            return Some(d.and_utc().timestamp());
        }
    }
    NaiveDate::parse_from_str(&s[..10.min(s.len())], "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp())
}

fn bool01(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

// ------------------------------------------------------------------ bodies

/// Body preferences of a request (`Options/BodyPreference`).
#[derive(Debug, Clone, Default)]
pub struct BodyPrefs {
    /// (type, truncation size, all-or-none)
    pub prefs: Vec<(u8, Option<usize>, bool)>,
    /// 0 never MIME, 1 S/MIME only, 2 always.
    pub mime_support: u8,
    pub mime_truncation: Option<usize>,
}

impl BodyPrefs {
    pub fn parse(options: Option<&Element>) -> BodyPrefs {
        let mut p = BodyPrefs::default();
        let Some(o) = options else { return p };
        for bp in o.children_named("AirSyncBase:BodyPreference") {
            let t = bp.text_of("Type").and_then(|t| t.trim().parse().ok()).unwrap_or(1);
            let size = bp.text_of("TruncationSize").and_then(|t| t.trim().parse().ok());
            let all = bp.text_of("AllOrNone").is_some_and(|v| v.trim() == "1");
            p.prefs.push((t, size, all));
        }
        p.mime_support = o.text_of("AirSync:MIMESupport").and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        p.mime_truncation = o.text_of("AirSync:MIMETruncation").and_then(|v| v.trim().parse::<u32>().ok()).map(|v| match v {
            0 => 0,
            1 => 4096,
            2 => 5120,
            3 => 7168,
            4 => 10240,
            5 => 20480,
            6 => 51200,
            7 => 102_400,
            _ => usize::MAX,
        });
        p
    }

    fn pref(&self, t: u8) -> Option<(Option<usize>, bool)> {
        self.prefs.iter().find(|(x, _, _)| *x == t).map(|(_, s, a)| (*s, *a))
    }
}

fn truncate(data: &str, limit: Option<usize>) -> (String, bool) {
    match limit {
        Some(l) if data.len() > l => {
            let mut end = l;
            while !data.is_char_boundary(end) {
                end -= 1;
            }
            (data[..end].to_string(), true)
        }
        _ => (data.to_string(), false),
    }
}

/// `AirSyncBase:Body` for the given content.
pub fn body(prefs: &BodyPrefs, text: &str, html: Option<&str>, mime_raw: Option<&[u8]>, smime: bool) -> Element {
    let want_mime = mime_raw.is_some() && (prefs.mime_support == 2 || (prefs.mime_support == 1 && smime)) && (prefs.pref(4).is_some() || prefs.prefs.is_empty());
    let mut b = Element::new("AirSyncBase:Body");
    if want_mime {
        let raw = mime_raw.expect("checked");
        let limit = prefs.pref(4).and_then(|p| p.0).or(prefs.mime_truncation);
        let truncated = limit.is_some_and(|l| raw.len() > l);
        let data = if truncated { &raw[..limit.expect("checked")] } else { raw };
        b.push_text("AirSyncBase:Type", "4");
        b.push_text("AirSyncBase:EstimatedDataSize", raw.len().to_string());
        if truncated {
            b.push_text("AirSyncBase:Truncated", "1");
        }
        b.push(Element::new("AirSyncBase:Data").opaque(data.to_vec()));
        return b;
    }
    let html_ok = prefs.pref(2).is_some();
    let (t, content, limit, all_or_none) = match html {
        Some(h) if html_ok => {
            let (l, a) = prefs.pref(2).expect("checked");
            (2, h.to_string(), l, a)
        }
        _ if html_ok && prefs.pref(1).is_none() => {
            let (l, a) = prefs.pref(2).expect("checked");
            (2, mime::text_to_html(text), l, a)
        }
        _ => {
            let (l, a) = prefs.pref(1).unwrap_or((None, false));
            (1, text.to_string(), l, a)
        }
    };
    b.push_text("AirSyncBase:Type", t.to_string());
    b.push_text("AirSyncBase:EstimatedDataSize", content.len().to_string());
    let (data, truncated) = truncate(&content, limit);
    if truncated && all_or_none {
        b.push_text("AirSyncBase:Truncated", "1");
        return b;
    }
    if truncated {
        b.push_text("AirSyncBase:Truncated", "1");
    }
    b.push(Element::new("AirSyncBase:Data").text(data));
    b
}

/// Plain text of a `Body` element sent by a client (type 1 or 2).
pub fn body_text(app_data: &Element) -> Option<String> {
    if let Some(b) = app_data.child("AirSyncBase:Body") {
        let data = b.text_of("Data").unwrap_or_default();
        return Some(if b.text_of("Type").as_deref() == Some("2") { mime::html_to_text(&data) } else { data });
    }
    // ActiveSync 2.5 style.
    app_data.elements().find(|e| e.tag == "Body" && e.page != 17).map(|e| e.content())
}

// ------------------------------------------------------------------- email

fn addr_list(list: &[Person]) -> String {
    mime::format_list(list)
}

/// Value of `Email:InternetCPID` etc. are fixed: UTF-8.
pub fn email(item: &Item, raw: Option<&[u8]>, prefs: &BodyPrefs, version: f32, me: &str) -> Element {
    let mut d = Element::new("AirSync:ApplicationData");
    let r = &item.recipients;
    if !r.to.is_empty() {
        d.push_text("Email:To", addr_list(&r.to));
    }
    if !r.cc.is_empty() {
        d.push_text("Email:Cc", addr_list(&r.cc));
    }
    if let Some(f) = &item.sender {
        d.push_text("Email:From", mime::format_person(f));
    }
    d.push_text("Email:Subject", item.subject.clone());
    if !r.reply_to.is_empty() {
        d.push_text("Email:ReplyTo", addr_list(&r.reply_to));
    }
    d.push_text("Email:DateReceived", iso(item.received));
    d.push_text("Email:DisplayTo", r.to.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("; "));
    d.push_text("Email:ThreadTopic", item.subject.clone());
    d.push_text("Email:Importance", item.importance.eas().to_string());
    d.push_text("Email:Read", bool01(item.flags & flags::READ != 0));
    if let Some(raw) = raw {
        let atts = mime::attachments(raw);
        if !atts.is_empty() {
            let mut list = Element::new("AirSyncBase:Attachments");
            for a in atts {
                let mut e = Element::new("AirSyncBase:Attachment")
                    .with_text("AirSyncBase:DisplayName", a.name.clone())
                    .with_text("AirSyncBase:FileReference", format!("{}:{}", item.id, a.index))
                    .with_text("AirSyncBase:Method", "1")
                    .with_text("AirSyncBase:EstimatedDataSize", a.size.to_string());
                if let Some(cid) = &a.content_id {
                    e.push_text("AirSyncBase:ContentId", cid.clone());
                }
                if a.inline {
                    e.push_text("AirSyncBase:IsInline", "1");
                }
                list.push(e);
            }
            d.push(list);
        }
        let (text, html) = mime::bodies(raw);
        let smime = item.message_class.contains("SMIME");
        d.push(body(prefs, text.as_deref().unwrap_or_default(), html.as_deref(), Some(raw), smime));
    }
    d.push_text("Email:MessageClass", if item.message_class.is_empty() { "IPM.Note".to_string() } else { item.message_class.clone() });
    if (item.message_class.starts_with("IPM.Schedule.Meeting.Request") || item.message_class.starts_with("IPM.Schedule.Meeting.Canceled"))
        && let Some(e) = item.data.as_deref().map(crate::ical::parse).and_then(|c| c.events.into_iter().next()) {
            d.push(meeting_request(&e, me));
        }
    d.push_text("Email:InternetCPID", "65001");
    let mut flag = Element::new("Email:Flag");
    if item.flags & flags::FLAGGED != 0 {
        flag.push_text("Email:FlagStatus", "2");
        flag.push_text("Email:FlagType", "for Follow Up");
    } else if item.flags & flags::COMPLETE != 0 {
        flag.push_text("Email:FlagStatus", "1");
    }
    d.push(flag);
    d.push_text("Email:ContentClass", if item.message_class.starts_with("IPM.Schedule") { "urn:content-classes:calendarmessage" } else { "urn:content-classes:message" });
    d.push_text("AirSyncBase:NativeBodyType", "2");
    if version >= 14.0 {
        let mut cats = Element::new("Email:Categories");
        for c in &item.categories {
            cats.push_text("Email:Category", c.clone());
        }
        d.push(cats);
        // Conversation id: hash of the thread root (References/In-Reply-To/
        // Message-ID), so clients group the same conversations as webmail;
        // falls back to the normalized subject when no message-ids are present.
        let digest = <sha2::Sha256 as sha2::Digest>::digest(thread_key(item).as_bytes());
        d.push(Element::new("Email2:ConversationId").opaque(digest[..16].to_vec()));
        d.push(Element::new("Email2:ConversationIndex").opaque(digest[16..21].to_vec()));
        if item.flags & flags::ANSWERED != 0 {
            d.push_text("Email2:LastVerbExecuted", "1");
        } else if item.flags & flags::FORWARDED != 0 {
            d.push_text("Email2:LastVerbExecuted", "3");
        }
    }
    d
}

/// Changes of a mail item that only touched flags/categories.
pub fn email_flags(item: &Item, version: f32) -> Element {
    let mut d = Element::new("AirSync:ApplicationData");
    d.push_text("Email:Read", bool01(item.flags & flags::READ != 0));
    let mut flag = Element::new("Email:Flag");
    if item.flags & flags::FLAGGED != 0 {
        flag.push_text("Email:FlagStatus", "2");
        flag.push_text("Email:FlagType", "for Follow Up");
    } else if item.flags & flags::COMPLETE != 0 {
        flag.push_text("Email:FlagStatus", "1");
    }
    d.push(flag);
    if version >= 14.0 {
        let mut cats = Element::new("Email:Categories");
        for c in &item.categories {
            cats.push_text("Email:Category", c.clone());
        }
        d.push(cats);
    }
    d
}

/// Flag changes sent by a client: (set, clear, categories).
pub fn email_changes(app_data: &Element) -> (i64, i64, Option<Vec<String>>) {
    let (mut set, mut clear) = (0, 0);
    if let Some(r) = app_data.text_of("Email:Read") {
        if r.trim() == "1" {
            set |= flags::READ;
        } else {
            clear |= flags::READ;
        }
    }
    if let Some(f) = app_data.child("Email:Flag") {
        match f.text_of("Email:FlagStatus").as_deref().map(str::trim) {
            Some("2") => {
                set |= flags::FLAGGED;
                clear |= flags::COMPLETE;
            }
            Some("1") => {
                set |= flags::COMPLETE;
                clear |= flags::FLAGGED;
            }
            _ => clear |= flags::FLAGGED | flags::COMPLETE,
        }
    }
    let cats = app_data.child("Email:Categories").map(|c| c.children_named("Email:Category").map(|x| x.content()).collect());
    (set, clear, cats)
}

/// Outlook's GlobalObjectId for a UID (MS-OXOCAL 2.2.1.27, "vCal-Uid" form).
pub fn global_obj_id(uid: &str) -> String {
    let mut b: Vec<u8> = vec![0x04, 0x00, 0x00, 0x00, 0x82, 0x00, 0xE0, 0x00, 0x74, 0xC5, 0xB7, 0x10, 0x1A, 0x82, 0xE0, 0x08];
    b.extend_from_slice(&[0; 4]); // instance date
    b.extend_from_slice(&[0; 8]); // creation time
    b.extend_from_slice(&[0; 8]);
    let mut data = b"vCal-Uid".to_vec();
    data.extend_from_slice(&1u32.to_le_bytes());
    data.extend_from_slice(uid.as_bytes());
    data.push(0);
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(&data);
    B64.encode(b)
}

fn meeting_request(e: &Event, me: &str) -> Element {
    let tz = e.zone();
    let mut m = Element::new("Email:MeetingRequest")
        .with_text("Email:AllDayEvent", bool01(e.all_day))
        .with_text("Email:StartTime", iso(e.start))
        .with_text("Email:DTStamp", iso(if e.dtstamp > 0 { e.dtstamp } else { Utc::now().timestamp() }))
        .with_text("Email:EndTime", iso(e.end))
        .with_text("Email:InstanceType", if e.recurrence.is_some() { "1" } else { "0" })
        .with_text("Email:Location", e.location.clone());
    if let Some(o) = &e.organizer {
        m.push_text("Email:Organizer", mime::format_person(o));
    }
    if let Some(r) = e.reminder {
        m.push_text("Email:Reminder", (r.max(0) * 60).to_string());
    }
    m.push_text("Email:ResponseRequested", bool01(!e.cancelled && e.organizer.as_ref().is_none_or(|o| !o.email.eq_ignore_ascii_case(me))));
    if let Some(r) = &e.recurrence {
        let mut rs = Element::new("Email:Recurrences");
        let mut x = Element::new("Email:Recurrence").with_text("Email:Type", r.freq.eas().to_string()).with_text("Email:Interval", r.interval.to_string());
        recurrence_fields(&mut x, r, "Email");
        rs.push(x);
        m.push(rs);
    }
    m.push_text("Email:Sensitivity", e.sensitivity.eas().to_string());
    m.push_text("Email:BusyStatus", if e.cancelled { "0".into() } else { BusyStatus::Busy.eas().to_string() });
    m.push_text("Email:TimeZone", tzinfo::eas_blob(tz));
    m.push_text("Email:GlobalObjId", global_obj_id(&e.uid));
    m
}

fn recurrence_fields(x: &mut Element, r: &Recurrence, ns: &str) {
    if let Some(u) = r.until {
        x.push_text(&format!("{ns}:Until"), if ns == "Email" { iso(u) } else { compact(u) });
    }
    if let Some(c) = r.count {
        x.push_text(&format!("{ns}:Occurrences"), c.to_string());
    }
    match r.freq {
        Frequency::Daily if r.days != 0 => x.push_text(&format!("{ns}:DayOfWeek"), r.days.to_string()),
        Frequency::Daily => {}
        Frequency::Weekly => x.push_text(&format!("{ns}:DayOfWeek"), r.days.to_string()),
        Frequency::Monthly => x.push_text(&format!("{ns}:DayOfMonth"), r.day_of_month.to_string()),
        Frequency::MonthlyNth => {
            x.push_text(&format!("{ns}:WeekOfMonth"), r.week_of_month.to_string());
            x.push_text(&format!("{ns}:DayOfWeek"), r.days.to_string());
        }
        Frequency::Yearly => {
            x.push_text(&format!("{ns}:DayOfMonth"), r.day_of_month.to_string());
            x.push_text(&format!("{ns}:MonthOfYear"), r.month.to_string());
        }
        Frequency::YearlyNth => {
            x.push_text(&format!("{ns}:WeekOfMonth"), r.week_of_month.to_string());
            x.push_text(&format!("{ns}:DayOfWeek"), r.days.to_string());
            x.push_text(&format!("{ns}:MonthOfYear"), r.month.to_string());
        }
    }
}

// ---------------------------------------------------------------- calendar

fn attendee_status(r: Response) -> &'static str {
    match r {
        Response::Tentative => "2",
        Response::Accepted | Response::Organizer => "3",
        Response::Declined => "4",
        Response::NotResponded => "5",
        Response::None => "0",
    }
}

fn response_type(r: Response) -> &'static str {
    match r {
        Response::None => "0",
        Response::Organizer => "1",
        Response::Tentative => "2",
        Response::Accepted => "3",
        Response::Declined => "4",
        Response::NotResponded => "5",
    }
}

fn parse_attendee_status(s: &str) -> Response {
    match s.trim() {
        "2" => Response::Tentative,
        "3" => Response::Accepted,
        "4" => Response::Declined,
        "5" => Response::NotResponded,
        _ => Response::None,
    }
}

pub fn is_organizer(e: &Event, me: &[String]) -> bool {
    e.organizer.as_ref().is_none_or(|o| o.email.is_empty() || me.iter().any(|m| m.eq_ignore_ascii_case(&o.email)))
}

pub fn event(e: &Event, prefs: &BodyPrefs, version: f32, me: &[String], user: &Person) -> Element {
    let tz = e.zone();
    let organizer = is_organizer(e, me);
    let mut d = Element::new("AirSync:ApplicationData");
    d.push_text("Calendar:TimeZone", tzinfo::eas_blob(tz));
    d.push_text("Calendar:AllDayEvent", bool01(e.all_day));
    d.push(body(prefs, &e.body, e.body_html.as_deref(), None, false));
    d.push_text("Calendar:BusyStatus", e.busy.eas().to_string());
    let org = e.organizer.clone().unwrap_or_else(|| user.clone());
    d.push_text("Calendar:OrganizerName", org.display().to_string());
    d.push_text("Calendar:OrganizerEmail", org.email.clone());
    d.push_text("Calendar:DTStamp", compact(if e.dtstamp > 0 { e.dtstamp } else { Utc::now().timestamp() }));
    d.push_text("Calendar:EndTime", compact(e.end));
    d.push_text("Calendar:Location", e.location.clone());
    if let Some(r) = e.reminder {
        d.push_text("Calendar:Reminder", r.max(0).to_string());
    }
    d.push_text("Calendar:Sensitivity", e.sensitivity.eas().to_string());
    d.push_text("Calendar:Subject", e.subject.clone());
    d.push_text("Calendar:StartTime", compact(e.start));
    d.push_text("Calendar:UID", e.uid.clone());
    let meeting = !e.attendees.is_empty() || !organizer;
    let status = match (meeting, organizer, e.cancelled) {
        (false, _, _) => "0",
        (true, true, false) => "1",
        (true, true, true) => "5",
        (true, false, false) => "3",
        (true, false, true) => "7",
    };
    d.push_text("Calendar:MeetingStatus", status);
    if !e.attendees.is_empty() {
        let mut list = Element::new("Calendar:Attendees");
        for a in &e.attendees {
            let mut x = Element::new("Calendar:Attendee").with_text("Calendar:Email", a.person.email.clone()).with_text("Calendar:Name", a.person.display().to_string());
            x.push_text("Calendar:AttendeeStatus", attendee_status(a.status));
            x.push_text(
                "Calendar:AttendeeType",
                match a.kind {
                    AttendeeKind::Required => "1",
                    AttendeeKind::Optional => "2",
                    AttendeeKind::Resource => "3",
                },
            );
            list.push(x);
        }
        d.push(list);
    }
    let mut cats = Element::new("Calendar:Categories");
    for c in &e.categories {
        cats.push_text("Calendar:Category", c.clone());
    }
    d.push(cats);
    if let Some(r) = &e.recurrence {
        let mut x = Element::new("Calendar:Recurrence").with_text("Calendar:Type", r.freq.eas().to_string()).with_text("Calendar:Interval", r.interval.to_string());
        recurrence_fields(&mut x, r, "Calendar");
        if version >= 14.1 && r.freq == Frequency::Weekly {
            x.push_text("Calendar:FirstDayOfWeek", r.first_day_of_week.to_string());
        }
        d.push(x);
        if !e.exceptions.is_empty() {
            let mut list = Element::new("Calendar:Exceptions");
            for ex in &e.exceptions {
                let mut x = Element::new("Calendar:Exception");
                if ex.deleted {
                    x.push_text("Calendar:Deleted", "1");
                }
                x.push_text("Calendar:ExceptionStartTime", compact(ex.original_start));
                if !ex.deleted {
                    if let Some(s) = &ex.subject {
                        x.push_text("Calendar:Subject", s.clone());
                    }
                    x.push_text("Calendar:StartTime", compact(ex.start.unwrap_or(ex.original_start)));
                    x.push_text("Calendar:EndTime", compact(ex.end.unwrap_or(ex.original_start + (e.end - e.start))));
                    if let Some(l) = &ex.location {
                        x.push_text("Calendar:Location", l.clone());
                    }
                    if let Some(b) = ex.busy {
                        x.push_text("Calendar:BusyStatus", b.eas().to_string());
                    }
                    if let Some(Some(r)) = ex.reminder {
                        x.push_text("Calendar:Reminder", r.to_string());
                    }
                    if let Some(b) = &ex.body {
                        x.push(body(prefs, b, None, None, false));
                    }
                }
                list.push(x);
            }
            d.push(list);
        }
    }
    if version >= 14.0 {
        if !organizer {
            d.push_text("Calendar:ResponseRequested", "1");
        }
        d.push_text("Calendar:ResponseType", response_type(if organizer && meeting { Response::Organizer } else { e.response }));
        d.push_text("Calendar:DisallowNewTimeProposal", "0");
    }
    if version >= 14.1
        && let Some(url) = &e.online_meeting_url
    {
        d.push_text("Calendar:OnlineMeetingExternalLink", url.clone());
    }
    d
}

fn parse_recurrence(x: &Element, ns: &str) -> Option<Recurrence> {
    let t = |n: &str| x.text_of(&format!("{ns}:{n}"));
    let freq = Frequency::from_eas(&t("Type")?)?;
    Some(Recurrence {
        freq,
        interval: t("Interval").and_then(|v| v.trim().parse().ok()).unwrap_or(1).max(1),
        days: t("DayOfWeek").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        day_of_month: t("DayOfMonth").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        week_of_month: t("WeekOfMonth").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        month: t("MonthOfYear").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
        until: t("Until").and_then(|v| parse_date(&v)),
        count: t("Occurrences").and_then(|v| v.trim().parse().ok()),
        first_day_of_week: t("FirstDayOfWeek").and_then(|v| v.trim().parse().ok()).unwrap_or(0),
    })
}

/// Apply calendar `ApplicationData` from a client onto `base` (a new event
/// or the stored one for a change).
pub fn apply_event(d: &Element, mut e: Event, user: &Person) -> Event {
    let t = |n: &str| d.text_of(&format!("Calendar:{n}"));
    if let Some(blob) = t("TimeZone")
        && let Some(rule) = tzinfo::parse_eas_blob(&blob)
    {
        e.tz = tzinfo::zone_for_rule(&rule).name().to_string();
    }
    if e.tz.is_empty() {
        e.tz = model::default_tz().name().to_string();
    }
    if let Some(v) = t("AllDayEvent") {
        e.all_day = v.trim() == "1";
    }
    if let Some(v) = body_text(d) {
        e.body = v;
        e.body_html = None;
    }
    if let Some(v) = t("BusyStatus") {
        e.busy = BusyStatus::from_eas(&v);
    }
    if let Some(v) = t("StartTime").and_then(|v| parse_date(&v)) {
        e.start = v;
    }
    if let Some(v) = t("EndTime").and_then(|v| parse_date(&v)) {
        e.end = v;
    }
    if e.end < e.start {
        e.end = e.start;
    }
    if let Some(v) = t("Location") {
        e.location = v;
    }
    match t("Reminder") {
        Some(v) => e.reminder = v.trim().parse().ok(),
        None if d.has("Calendar:Subject") => e.reminder = None,
        None => {}
    }
    if let Some(v) = t("Sensitivity") {
        e.sensitivity = Sensitivity::from_eas(&v);
    }
    if let Some(v) = t("Subject") {
        e.subject = v;
    }
    if let Some(v) = t("UID").filter(|u| !u.is_empty()) {
        e.uid = v;
    }
    if e.uid.is_empty() {
        e.uid = uuid::Uuid::new_v4().to_string().to_uppercase();
    }
    if let Some(v) = t("DTStamp").and_then(|v| parse_date(&v)) {
        e.dtstamp = v;
    }
    let email = t("OrganizerEmail");
    if let Some(email) = email.filter(|m| !m.is_empty()) {
        e.organizer = Some(Person::new(t("OrganizerName").unwrap_or_default(), email));
    }
    if let Some(list) = d.child("Calendar:Attendees") {
        let old = std::mem::take(&mut e.attendees);
        for a in list.children_named("Calendar:Attendee") {
            let email = a.text_of("Calendar:Email").unwrap_or_default();
            let prev = old.iter().find(|o| o.person.email.eq_ignore_ascii_case(&email));
            e.attendees.push(Attendee {
                person: Person::new(a.text_of("Calendar:Name").unwrap_or_default(), email.clone()),
                kind: match a.text_of("Calendar:AttendeeType").as_deref().map(str::trim) {
                    Some("2") => AttendeeKind::Optional,
                    Some("3") => AttendeeKind::Resource,
                    _ => AttendeeKind::Required,
                },
                status: a
                    .text_of("Calendar:AttendeeStatus")
                    .map(|s| parse_attendee_status(&s))
                    .filter(|s| *s != Response::None)
                    .or(prev.map(|p| p.status))
                    .unwrap_or(Response::NotResponded),
            });
        }
    } else if d.has("Calendar:Subject") {
        // A full item without attendees: not (or no longer) a meeting.
        if e.organizer.as_ref().is_none_or(|o| o.email.eq_ignore_ascii_case(&user.email)) {
            e.attendees.clear();
        }
    }
    if e.organizer.is_none() && !e.attendees.is_empty() {
        e.organizer = Some(user.clone());
    }
    if let Some(c) = d.child("Calendar:Categories") {
        e.categories = c.children_named("Calendar:Category").map(|x| x.content()).collect();
    }
    if let Some(s) = t("MeetingStatus") {
        e.cancelled = matches!(s.trim(), "5" | "7" | "13" | "15");
    }
    if let Some(r) = t("ResponseType") {
        e.response = match r.trim() {
            "1" => Response::Organizer,
            "2" => Response::Tentative,
            "3" => Response::Accepted,
            "4" => Response::Declined,
            "5" => Response::NotResponded,
            _ => e.response,
        };
    }
    match d.child("Calendar:Recurrence") {
        Some(x) => {
            e.recurrence = parse_recurrence(x, "Calendar").map(|mut r| {
                r.normalize(e.start_local());
                r
            })
        }
        None if d.has("Calendar:Subject") => e.recurrence = None,
        None => {}
    }
    if let Some(list) = d.child("Calendar:Exceptions") {
        let duration = e.end - e.start;
        let mut exceptions = Vec::new();
        for x in list.children_named("Calendar:Exception") {
            let Some(original) = x.text_of("Calendar:ExceptionStartTime").and_then(|v| parse_date(&v)) else { continue };
            let mut ex = Exception { original_start: original, ..Default::default() };
            ex.deleted = x.text_of("Calendar:Deleted").is_some_and(|v| v.trim() == "1");
            if !ex.deleted {
                ex.subject = x.text_of("Calendar:Subject");
                ex.location = x.text_of("Calendar:Location");
                ex.start = x.text_of("Calendar:StartTime").and_then(|v| parse_date(&v)).or(Some(original));
                ex.end = x.text_of("Calendar:EndTime").and_then(|v| parse_date(&v)).or(Some(original + duration));
                ex.busy = x.text_of("Calendar:BusyStatus").map(|v| BusyStatus::from_eas(&v));
                ex.reminder = x.text_of("Calendar:Reminder").map(|v| v.trim().parse().ok());
                ex.body = body_text(x);
                ex.all_day = x.text_of("Calendar:AllDayEvent").map(|v| v.trim() == "1");
                ex.cancelled = x.text_of("Calendar:MeetingStatus").map(|s| matches!(s.trim(), "5" | "7"));
            }
            exceptions.push(ex);
        }
        e.exceptions = exceptions;
    } else if d.has("Calendar:Subject") && e.recurrence.is_none() {
        e.exceptions.clear();
    }
    e
}

// ---------------------------------------------------------------- contacts

fn opt_push(d: &mut Element, name: &str, v: &str) {
    if !v.is_empty() {
        d.push_text(name, v.to_string());
    }
}

fn date_only_iso(s: &str) -> String {
    format!("{}T00:00:00.000Z", &s[..10.min(s.len())])
}

pub fn contact(c: &Contact, prefs: &BodyPrefs, version: f32) -> Element {
    let mut d = Element::new("AirSync:ApplicationData");
    if let Some(b) = &c.anniversary {
        d.push_text("Contacts:Anniversary", date_only_iso(b));
    }
    opt_push(&mut d, "Contacts:AssistantName", &c.assistant);
    opt_push(&mut d, "Contacts:AssistantPhoneNumber", &c.assistant_phone);
    if let Some(b) = &c.birthday {
        d.push_text("Contacts:Birthday", date_only_iso(b));
    }
    if !c.notes.is_empty() {
        d.push(body(prefs, &c.notes, None, None, false));
    }
    opt_push(&mut d, "Contacts:Business2PhoneNumber", &c.business_phone2);
    let addr = |d: &mut Element, prefix: &str, a: &Address| {
        opt_push(d, &format!("Contacts:{prefix}City"), &a.city);
        opt_push(d, &format!("Contacts:{prefix}Country"), &a.country);
        opt_push(d, &format!("Contacts:{prefix}PostalCode"), &a.postal_code);
        opt_push(d, &format!("Contacts:{prefix}State"), &a.state);
        opt_push(d, &format!("Contacts:{prefix}Street"), &a.street);
    };
    addr(&mut d, "Business", &c.business_address);
    opt_push(&mut d, "Contacts:BusinessFaxNumber", &c.business_fax);
    opt_push(&mut d, "Contacts:BusinessPhoneNumber", &c.business_phone);
    opt_push(&mut d, "Contacts:CarPhoneNumber", &c.car_phone);
    if !c.categories.is_empty() {
        let mut cats = Element::new("Contacts:Categories");
        for x in &c.categories {
            cats.push_text("Contacts:Category", x.clone());
        }
        d.push(cats);
    }
    if !c.children.is_empty() {
        let mut ch = Element::new("Contacts:Children");
        for x in &c.children {
            ch.push_text("Contacts:Child", x.clone());
        }
        d.push(ch);
    }
    opt_push(&mut d, "Contacts:CompanyName", &c.company);
    opt_push(&mut d, "Contacts:Department", &c.department);
    for (i, e) in c.emails.iter().take(3).enumerate() {
        d.push_text(&format!("Contacts:Email{}Address", i + 1), e.clone());
    }
    d.push_text("Contacts:FileAs", if c.file_as.is_empty() { c.effective_name() } else { c.file_as.clone() });
    opt_push(&mut d, "Contacts:FirstName", &c.first_name);
    opt_push(&mut d, "Contacts:Home2PhoneNumber", &c.home_phone2);
    addr(&mut d, "Home", &c.home_address);
    opt_push(&mut d, "Contacts:HomeFaxNumber", &c.home_fax);
    opt_push(&mut d, "Contacts:HomePhoneNumber", &c.home_phone);
    opt_push(&mut d, "Contacts:JobTitle", &c.job_title);
    opt_push(&mut d, "Contacts:LastName", &c.last_name);
    opt_push(&mut d, "Contacts:MiddleName", &c.middle_name);
    opt_push(&mut d, "Contacts:MobilePhoneNumber", &c.mobile_phone);
    opt_push(&mut d, "Contacts:OfficeLocation", &c.office);
    addr(&mut d, "Other", &c.other_address);
    opt_push(&mut d, "Contacts:PagerNumber", &c.pager);
    opt_push(&mut d, "Contacts:RadioPhoneNumber", &c.radio_phone);
    opt_push(&mut d, "Contacts:Spouse", &c.spouse);
    opt_push(&mut d, "Contacts:Suffix", &c.suffix);
    opt_push(&mut d, "Contacts:Title", &c.title);
    opt_push(&mut d, "Contacts:WebPage", &c.web_page);
    if let Some(p) = &c.picture {
        d.push_text("Contacts:Picture", p.clone());
    }
    for (i, im) in c.im.iter().take(3).enumerate() {
        d.push_text(if i == 0 { "Contacts2:IMAddress".to_string() } else { format!("Contacts2:IMAddress{}", i + 1) }.as_str(), im.clone());
    }
    opt_push(&mut d, "Contacts2:ManagerName", &c.manager);
    opt_push(&mut d, "Contacts2:CompanyMainPhone", &c.company_main_phone);
    opt_push(&mut d, "Contacts2:NickName", &c.nickname);
    let _ = version;
    d
}

pub fn apply_contact(d: &Element, mut c: Contact) -> Contact {
    // ActiveSync sends complete contacts; missing fields are empty.
    let full = d.has("Contacts:FileAs") || d.has("Contacts:FirstName") || d.has("Contacts:LastName") || d.has("Contacts:CompanyName");
    let t = |n: &str| d.text_of(n);
    let set = |field: &mut String, name: &str| match t(name) {
        Some(v) => *field = v,
        None if full => field.clear(),
        None => {}
    };
    set(&mut c.assistant, "Contacts:AssistantName");
    set(&mut c.assistant_phone, "Contacts:AssistantPhoneNumber");
    set(&mut c.business_phone2, "Contacts:Business2PhoneNumber");
    set(&mut c.business_address.city, "Contacts:BusinessCity");
    set(&mut c.business_address.country, "Contacts:BusinessCountry");
    set(&mut c.business_address.postal_code, "Contacts:BusinessPostalCode");
    set(&mut c.business_address.state, "Contacts:BusinessState");
    set(&mut c.business_address.street, "Contacts:BusinessStreet");
    set(&mut c.business_fax, "Contacts:BusinessFaxNumber");
    set(&mut c.business_phone, "Contacts:BusinessPhoneNumber");
    set(&mut c.car_phone, "Contacts:CarPhoneNumber");
    set(&mut c.company, "Contacts:CompanyName");
    set(&mut c.department, "Contacts:Department");
    set(&mut c.file_as, "Contacts:FileAs");
    set(&mut c.first_name, "Contacts:FirstName");
    set(&mut c.home_phone2, "Contacts:Home2PhoneNumber");
    set(&mut c.home_address.city, "Contacts:HomeCity");
    set(&mut c.home_address.country, "Contacts:HomeCountry");
    set(&mut c.home_address.postal_code, "Contacts:HomePostalCode");
    set(&mut c.home_address.state, "Contacts:HomeState");
    set(&mut c.home_address.street, "Contacts:HomeStreet");
    set(&mut c.home_fax, "Contacts:HomeFaxNumber");
    set(&mut c.home_phone, "Contacts:HomePhoneNumber");
    set(&mut c.job_title, "Contacts:JobTitle");
    set(&mut c.last_name, "Contacts:LastName");
    set(&mut c.middle_name, "Contacts:MiddleName");
    set(&mut c.mobile_phone, "Contacts:MobilePhoneNumber");
    set(&mut c.office, "Contacts:OfficeLocation");
    set(&mut c.other_address.city, "Contacts:OtherCity");
    set(&mut c.other_address.country, "Contacts:OtherCountry");
    set(&mut c.other_address.postal_code, "Contacts:OtherPostalCode");
    set(&mut c.other_address.state, "Contacts:OtherState");
    set(&mut c.other_address.street, "Contacts:OtherStreet");
    set(&mut c.pager, "Contacts:PagerNumber");
    set(&mut c.radio_phone, "Contacts:RadioPhoneNumber");
    set(&mut c.spouse, "Contacts:Spouse");
    set(&mut c.suffix, "Contacts:Suffix");
    set(&mut c.title, "Contacts:Title");
    set(&mut c.web_page, "Contacts:WebPage");
    set(&mut c.manager, "Contacts2:ManagerName");
    set(&mut c.company_main_phone, "Contacts2:CompanyMainPhone");
    set(&mut c.nickname, "Contacts2:NickName");
    let emails: Vec<String> = (1..=3)
        .filter_map(|i| t(&format!("Contacts:Email{i}Address")))
        .map(|e| mime::parse_meta(format!("To: {e}\r\n\r\n").as_bytes()).to.into_iter().next().map(|p| p.email).unwrap_or(e))
        .filter(|e| !e.is_empty())
        .collect();
    if !emails.is_empty() || full {
        c.emails = emails;
    }
    let ims: Vec<String> = ["Contacts2:IMAddress", "Contacts2:IMAddress2", "Contacts2:IMAddress3"].iter().filter_map(|n| t(n)).collect();
    if !ims.is_empty() || full {
        c.im = ims;
    }
    match t("Contacts:Birthday") {
        Some(b) => c.birthday = parse_date(&b).map(contact_date),
        None if full => c.birthday = None,
        None => {}
    }
    match t("Contacts:Anniversary") {
        Some(b) => c.anniversary = parse_date(&b).map(contact_date),
        None if full => c.anniversary = None,
        None => {}
    }
    if let Some(b) = body_text(d) {
        c.notes = b;
    } else if full {
        c.notes.clear();
    }
    if let Some(cats) = d.child("Contacts:Categories") {
        c.categories = cats.children_named("Contacts:Category").map(|x| x.content()).collect();
    } else if full {
        c.categories.clear();
    }
    if let Some(ch) = d.child("Contacts:Children") {
        c.children = ch.children_named("Contacts:Child").map(|x| x.content()).collect();
    } else if full {
        c.children.clear();
    }
    match t("Contacts:Picture") {
        Some(p) if !p.is_empty() => c.picture = Some(p),
        Some(_) => c.picture = None,
        None if full => c.picture = None,
        None => {}
    }
    if c.display_name.is_empty() || full {
        c.display_name = [c.first_name.as_str(), c.middle_name.as_str(), c.last_name.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
    }
    c
}

/// Birthdays arrive as UTC timestamps of local midnight (or 11:59 UTC);
/// round to the nearest date.
fn contact_date(ts: i64) -> String {
    DateTime::from_timestamp(ts + 12 * 3600, 0).unwrap_or_default().format("%Y-%m-%d").to_string()
}

// ------------------------------------------------------------------- tasks

pub fn task(t: &Task, prefs: &BodyPrefs) -> Element {
    let mut d = Element::new("AirSync:ApplicationData");
    if !t.body.is_empty() {
        d.push(body(prefs, &t.body, None, None, false));
    }
    d.push_text("Tasks:Subject", t.subject.clone());
    d.push_text("Tasks:Importance", t.importance.eas().to_string());
    if let Some(s) = t.start {
        d.push_text("Tasks:UtcStartDate", iso(s));
        d.push_text("Tasks:StartDate", iso(s));
    }
    if let Some(s) = t.due {
        d.push_text("Tasks:UtcDueDate", iso(s));
        d.push_text("Tasks:DueDate", iso(s));
    }
    let mut cats = Element::new("Tasks:Categories");
    for c in &t.categories {
        cats.push_text("Tasks:Category", c.clone());
    }
    d.push(cats);
    d.push_text("Tasks:Complete", bool01(t.complete));
    if t.complete {
        d.push_text("Tasks:DateCompleted", iso(t.completed_at.unwrap_or_else(|| Utc::now().timestamp())));
    }
    d.push_text("Tasks:Sensitivity", t.sensitivity.eas().to_string());
    d.push_text("Tasks:ReminderSet", bool01(t.reminder.is_some()));
    if let Some(r) = t.reminder {
        d.push_text("Tasks:ReminderTime", iso(r));
    }
    d
}

pub fn apply_task(d: &Element, mut t: Task) -> Task {
    let g = |n: &str| d.text_of(&format!("Tasks:{n}"));
    if let Some(v) = g("Subject") {
        t.subject = v;
    }
    if let Some(v) = body_text(d) {
        t.body = v;
    }
    if let Some(v) = g("Importance") {
        t.importance = Importance::from_eas(&v);
    }
    t.start = g("UtcStartDate").or(g("StartDate")).and_then(|v| parse_date(&v)).or(if d.has("Tasks:Subject") { None } else { t.start });
    t.due = g("UtcDueDate").or(g("DueDate")).and_then(|v| parse_date(&v)).or(if d.has("Tasks:Subject") { None } else { t.due });
    if let Some(v) = g("Complete") {
        t.complete = v.trim() == "1";
        t.percent_complete = if t.complete { 100 } else { t.percent_complete.min(99) };
    }
    t.completed_at = if t.complete { g("DateCompleted").and_then(|v| parse_date(&v)).or(t.completed_at).or(Some(Utc::now().timestamp())) } else { None };
    if let Some(v) = g("Sensitivity") {
        t.sensitivity = Sensitivity::from_eas(&v);
    }
    if let Some(v) = g("ReminderSet") {
        t.reminder = if v.trim() == "1" { g("ReminderTime").and_then(|r| parse_date(&r)).or(t.due) } else { None };
    }
    if let Some(c) = d.child("Tasks:Categories") {
        t.categories = c.children_named("Tasks:Category").map(|x| x.content()).collect();
    }
    t
}

// ------------------------------------------------------------------- notes

pub fn note(n: &Note, prefs: &BodyPrefs, modified: i64) -> Element {
    let mut d = Element::new("AirSync:ApplicationData");
    d.push(body(prefs, &n.body, None, None, false));
    d.push_text("Notes:Subject", n.subject.clone());
    d.push_text("Notes:MessageClass", "IPM.StickyNote");
    d.push_text("Notes:LastModifiedDate", iso(modified));
    let mut cats = Element::new("Notes:Categories");
    for c in &n.categories {
        cats.push_text("Notes:Category", c.clone());
    }
    d.push(cats);
    d
}

pub fn apply_note(d: &Element, mut n: Note) -> Note {
    if let Some(v) = d.text_of("Notes:Subject") {
        n.subject = v;
    }
    if let Some(v) = body_text(d) {
        n.body = v;
    }
    if n.subject.is_empty() {
        n.subject = n.body.lines().next().unwrap_or_default().chars().take(80).collect();
    }
    if let Some(c) = d.child("Notes:Categories") {
        n.categories = c.children_named("Notes:Category").map(|x| x.content()).collect();
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail_item(subject: &str, message_id: Option<&str>, in_reply_to: Option<&str>, references: Option<&str>) -> Item {
        Item {
            id: 1,
            user_id: 1,
            folder_id: 1,
            kind: crate::model::ItemKind::Mail,
            uid: None,
            modseq: 1,
            created_modseq: 1,
            received: 0,
            size: 0,
            flags: 0,
            categories: vec![],
            subject: subject.into(),
            sender: None,
            recipients: Default::default(),
            preview: String::new(),
            has_attachments: false,
            importance: Importance::Normal,
            message_id: message_id.map(str::to_string),
            message_class: "IPM.Note".into(),
            data: None,
            in_reply_to: in_reply_to.map(str::to_string),
            references: references.map(str::to_string),
            imap_uid: None,
        }
    }

    #[test]
    fn conversation_key_follows_references() {
        // A reply keys on the thread root (References first id), matching the
        // root's own Message-ID, so both land in one conversation.
        let root = mail_item("Hi", Some("<root@x>"), None, None);
        let reply = mail_item("Re: Hi", Some("<r1@x>"), Some("<root@x>"), Some("<root@x>"));
        assert_eq!(thread_key(&root), thread_key(&reply));
        // A different message with the same subject but no reference link keys on
        // its own Message-ID, so it is not merged by subject.
        let other = mail_item("Hi", Some("<z@x>"), None, None);
        assert_ne!(thread_key(&root), thread_key(&other));
        // With no message-ids at all, the normalized subject is the key.
        let a = mail_item("Re: Lunch?", None, None, None);
        let b = mail_item("Lunch?", None, None, None);
        assert_eq!(thread_key(&a), thread_key(&b));
    }

    #[test]
    fn calendar_round_trip() {
        let me = Person::new("Niklas", "niklas@example.com");
        let mut e = Event {
            uid: "U1".into(),
            subject: "Jour fixe".into(),
            start: 1_795_000_000,
            end: 1_795_003_600,
            tz: "Europe/Berlin".into(),
            reminder: Some(15),
            recurrence: Some(Recurrence { freq: Frequency::Weekly, interval: 1, days: 2, day_of_month: 0, week_of_month: 0, month: 0, until: None, count: Some(5), first_day_of_week: 1 }),
            ..Default::default()
        };
        e.exception_mut(1_795_000_000 + 7 * 86_400).deleted = true;
        let prefs = BodyPrefs { prefs: vec![(1, Some(200), false)], ..Default::default() };
        let d = event(&e, &prefs, 14.1, &["niklas@example.com".into()], &me);
        let back = Element::decode(&d.encode()).unwrap();
        assert_eq!(back.text_of("Calendar:StartTime").as_deref(), Some(compact(e.start).as_str()));
        let parsed = apply_event(&back, Event::default(), &me);
        assert_eq!(parsed.start, e.start);
        assert_eq!(parsed.tz, "Europe/Berlin");
        assert_eq!(parsed.recurrence, e.recurrence);
        assert_eq!(parsed.exceptions.len(), 1);
        assert!(parsed.exceptions[0].deleted);
        assert_eq!(parsed.reminder, Some(15));
    }

    #[test]
    fn contact_round_trip() {
        let c = Contact {
            first_name: "Max".into(),
            last_name: "Becker".into(),
            emails: vec!["max@example.com".into()],
            mobile_phone: "0170 1".into(),
            birthday: Some("1990-05-01".into()),
            business_address: Address { city: "Homburg".into(), ..Default::default() },
            ..Default::default()
        };
        let d = contact(&c, &BodyPrefs::default(), 14.1);
        let back = apply_contact(&d, Contact::default());
        assert_eq!(back.emails, c.emails);
        assert_eq!(back.birthday.as_deref(), Some("1990-05-01"));
        assert_eq!(back.business_address.city, "Homburg");
        assert_eq!(back.display_name, "Max Becker");
        // Outlook-style addresses with names.
        let mut d2 = Element::new("AirSync:ApplicationData").with_text("Contacts:FileAs", "X").with_text("Contacts:Email1Address", "\"Max\" <max@example.com>");
        d2.push_text("Contacts:Birthday", "1990-04-30T22:00:00.000Z");
        let b = apply_contact(&d2, Contact::default());
        assert_eq!(b.emails, vec!["max@example.com".to_string()]);
        assert_eq!(b.birthday.as_deref(), Some("1990-05-01"));
    }

    #[test]
    fn bodies_and_truncation() {
        let prefs = BodyPrefs { prefs: vec![(2, Some(5), false)], ..Default::default() };
        let b = body(&prefs, "Hallo Welt", Some("<p>Hallo Welt</p>"), None, false);
        assert_eq!(b.text_of("Type").as_deref(), Some("2"));
        assert_eq!(b.text_of("Truncated").as_deref(), Some("1"));
        assert_eq!(b.text_of("Data").as_deref(), Some("<p>Ha"));
        let mime_prefs = BodyPrefs { prefs: vec![(4, None, false)], mime_support: 2, mime_truncation: None };
        let b = body(&mime_prefs, "", None, Some(b"Subject: x\r\n\r\nHi"), false);
        assert_eq!(b.text_of("Type").as_deref(), Some("4"));
        assert_eq!(parse_date("2026-11-22T09:00:00.000Z"), parse_date("20261122T090000Z"));
        assert!(global_obj_id("abc").starts_with("BAAAAIIA4AB0xbcQGoLgCA"));
    }
}
