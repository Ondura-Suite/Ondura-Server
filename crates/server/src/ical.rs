//! iCalendar (RFC 5545) reading and writing for meeting messages (iTIP).

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::model::{
    Attendee, AttendeeKind, BusyStatus, Event, Exception, Frequency, Person, Recurrence, Response, Sensitivity, WEEKDAYS, day_bit, default_tz, local_midnight,
};
use crate::tzinfo;

#[derive(Debug, Clone, Default)]
pub struct Calendar {
    pub method: String,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone)]
struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            lines.last_mut().expect("not empty").push_str(&raw[1..]);
        } else if !raw.is_empty() {
            lines.push(raw.to_string());
        }
    }
    lines
}

fn parse_line(line: &str) -> Option<Prop> {
    // Split at the first ':' outside quotes.
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut q = false;
    for c in head.chars() {
        match c {
            '"' => q = !q,
            ';' if !q => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    let name = parts.remove(0).to_ascii_uppercase();
    let params = parts
        .into_iter()
        .filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())))
        .collect();
    Some(Prop { name, params, value: value.to_string() })
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// A date or date-time property: (UTC timestamp, is date only, zone).
fn parse_time(p: &Prop) -> Option<(i64, bool, Tz)> {
    let v = p.value.trim();
    let tz = p.param("TZID").and_then(|t| tzinfo::from_windows_name(t.trim_matches('"'))).unwrap_or_else(default_tz);
    if p.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || (v.len() == 8 && !v.contains('T')) {
        let d = NaiveDate::parse_from_str(&v[..8.min(v.len())], "%Y%m%d").ok()?;
        return Some((local_midnight(tz, d), true, tz));
    }
    if let Some(z) = v.strip_suffix('Z') {
        let dt = NaiveDateTime::parse_from_str(z, "%Y%m%dT%H%M%S").ok()?;
        return Some((dt.and_utc().timestamp(), false, if p.param("TZID").is_some() { tz } else { default_tz() }));
    }
    let dt = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
    let ts = tz.from_local_datetime(&dt).earliest().map(|d| d.timestamp()).unwrap_or_else(|| dt.and_utc().timestamp());
    Some((ts, false, tz))
}

fn parse_duration(s: &str) -> Option<i64> {
    // [+-]P[nW][nD][T[nH][nM][nS]]
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let s = s.strip_prefix('P')?;
    let mut total = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        match c {
            '0'..='9' => num.push(c),
            'T' => {}
            'W' | 'D' | 'H' | 'M' | 'S' => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match c {
                    'W' => 604_800,
                    'D' => 86_400,
                    'H' => 3600,
                    'M' => 60,
                    _ => 1,
                };
            }
            _ => return None,
        }
    }
    Some(if neg { -total } else { total })
}

fn parse_rrule(v: &str, start_local: NaiveDateTime, tz: Tz) -> Option<Recurrence> {
    let mut freq = None;
    let mut r = Recurrence { freq: Frequency::Daily, interval: 1, days: 0, day_of_month: 0, week_of_month: 0, month: 0, until: None, count: None, first_day_of_week: 1 };
    let mut byday: Vec<(i32, u8)> = Vec::new();
    let mut setpos: Option<i32> = None;
    for part in v.split(';') {
        let Some((k, val)) = part.split_once('=') else { continue };
        match k.to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(val.to_ascii_uppercase()),
            "INTERVAL" => r.interval = val.parse().unwrap_or(1),
            "COUNT" => r.count = val.parse().ok(),
            "UNTIL" => {
                let p = Prop { name: "UNTIL".into(), params: vec![], value: val.to_string() };
                r.until = parse_time(&p).map(|(t, date_only, _)| if date_only { t + 86_399 } else { t });
                let _ = tz;
            }
            "BYMONTHDAY" => r.day_of_month = val.split(',').next().and_then(|d| d.parse::<i32>().ok()).filter(|d| *d > 0).unwrap_or(0) as u32,
            "BYMONTH" => r.month = val.split(',').next().and_then(|d| d.parse().ok()).unwrap_or(0),
            "BYSETPOS" => setpos = val.split(',').next().and_then(|d| d.parse().ok()),
            "WKST" => r.first_day_of_week = DAYS.iter().position(|d| val.eq_ignore_ascii_case(d)).unwrap_or(1) as u8,
            "BYDAY" => {
                for d in val.split(',') {
                    let d = d.trim();
                    if d.len() < 2 {
                        continue;
                    }
                    let (n, day) = d.split_at(d.len() - 2);
                    let Some(idx) = DAYS.iter().position(|x| day.eq_ignore_ascii_case(x)) else { continue };
                    byday.push((n.parse().unwrap_or(0), day_bit(WEEKDAYS[idx])));
                }
            }
            _ => {}
        }
    }
    let mask: u8 = byday.iter().fold(0, |m, (_, b)| m | b);
    let ordinal = byday.iter().map(|(n, _)| *n).find(|n| *n != 0).or(setpos);
    let week = |n: i32| if n < 0 { 5 } else { n.clamp(1, 5) as u32 };
    r.freq = match freq?.as_str() {
        "DAILY" => {
            r.days = mask;
            Frequency::Daily
        }
        "WEEKLY" => {
            r.days = if mask == 0 { day_bit(start_local.weekday()) } else { mask };
            Frequency::Weekly
        }
        "MONTHLY" => match ordinal {
            Some(n) if mask != 0 => {
                r.days = mask;
                r.week_of_month = week(n);
                Frequency::MonthlyNth
            }
            _ => Frequency::Monthly,
        },
        "YEARLY" => match ordinal {
            Some(n) if mask != 0 => {
                r.days = mask;
                r.week_of_month = week(n);
                Frequency::YearlyNth
            }
            _ => Frequency::Yearly,
        },
        _ => return None,
    };
    r.normalize(start_local);
    Some(r)
}

const DAYS: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];

fn mailto(v: &str) -> String {
    let v = v.trim();
    v.strip_prefix("mailto:").or_else(|| v.strip_prefix("MAILTO:")).unwrap_or(v).to_ascii_lowercase()
}

fn partstat(s: Option<&str>) -> Response {
    match s.map(|s| s.to_ascii_uppercase()).as_deref() {
        Some("ACCEPTED") => Response::Accepted,
        Some("DECLINED") => Response::Declined,
        Some("TENTATIVE") => Response::Tentative,
        Some("NEEDS-ACTION") => Response::NotResponded,
        _ => Response::None,
    }
}

fn partstat_str(r: Response) -> &'static str {
    match r {
        Response::Accepted | Response::Organizer => "ACCEPTED",
        Response::Declined => "DECLINED",
        Response::Tentative => "TENTATIVE",
        Response::None | Response::NotResponded => "NEEDS-ACTION",
    }
}

#[derive(Default)]
struct RawEvent {
    props: Vec<Prop>,
    alarm_trigger: Option<String>,
}

pub fn parse(text: &str) -> Calendar {
    let mut cal = Calendar::default();
    let mut raw_events: Vec<RawEvent> = Vec::new();
    let mut depth: Vec<String> = Vec::new();
    for line in unfold(text) {
        let Some(p) = parse_line(&line) else { continue };
        match p.name.as_str() {
            "BEGIN" => {
                let c = p.value.trim().to_ascii_uppercase();
                if c == "VEVENT" {
                    raw_events.push(RawEvent::default());
                }
                depth.push(c);
            }
            "END" => {
                depth.pop();
            }
            "METHOD" if depth.len() == 1 => cal.method = p.value.trim().to_ascii_uppercase(),
            _ => match depth.last().map(String::as_str) {
                Some("VEVENT") => raw_events.last_mut().expect("in VEVENT").props.push(p),
                Some("VALARM") if depth.iter().any(|d| d == "VEVENT") && p.name == "TRIGGER" => {
                    raw_events.last_mut().expect("in VEVENT").alarm_trigger = Some(p.value.clone());
                }
                _ => {}
            },
        }
    }

    let mut masters: Vec<Event> = Vec::new();
    let mut overrides: Vec<(String, i64, Event)> = Vec::new();
    for raw in raw_events {
        let get = |n: &str| raw.props.iter().find(|p| p.name == n);
        let Some(uid) = get("UID").map(|p| p.value.trim().to_string()) else { continue };
        let Some((start, all_day, tz)) = get("DTSTART").and_then(parse_time) else { continue };
        let end = get("DTEND")
            .and_then(parse_time)
            .map(|t| t.0)
            .or_else(|| get("DURATION").and_then(|d| parse_duration(&d.value)).map(|d| start + d))
            .unwrap_or(if all_day { start + 86_400 } else { start });
        let mut e = Event {
            uid: uid.clone(),
            subject: get("SUMMARY").map(|p| unescape(&p.value)).unwrap_or_default(),
            location: get("LOCATION").map(|p| unescape(&p.value)).unwrap_or_default(),
            body: get("DESCRIPTION").map(|p| unescape(&p.value)).unwrap_or_default(),
            start,
            end,
            all_day,
            tz: tz.name().to_string(),
            sequence: get("SEQUENCE").and_then(|p| p.value.trim().parse().ok()).unwrap_or(0),
            cancelled: get("STATUS").is_some_and(|p| p.value.trim().eq_ignore_ascii_case("CANCELLED")),
            dtstamp: get("DTSTAMP").and_then(parse_time).map(|t| t.0).unwrap_or_else(|| Utc::now().timestamp()),
            ..Default::default()
        };
        e.sensitivity = match get("CLASS").map(|p| p.value.trim().to_ascii_uppercase()).as_deref() {
            Some("PRIVATE") => Sensitivity::Private,
            Some("CONFIDENTIAL") => Sensitivity::Confidential,
            _ => Sensitivity::Normal,
        };
        e.busy = match get("X-MICROSOFT-CDO-BUSYSTATUS").map(|p| p.value.trim().to_ascii_uppercase()).as_deref() {
            Some("FREE") => BusyStatus::Free,
            Some("TENTATIVE") => BusyStatus::Tentative,
            Some("OOF") => BusyStatus::Oof,
            Some("WORKINGELSEWHERE") => BusyStatus::WorkingElsewhere,
            Some(_) => BusyStatus::Busy,
            None if get("TRANSP").is_some_and(|p| p.value.trim().eq_ignore_ascii_case("TRANSPARENT")) => BusyStatus::Free,
            None => BusyStatus::Busy,
        };
        if let Some(cats) = get("CATEGORIES") {
            e.categories = cats.value.split(',').map(|c| unescape(c.trim())).filter(|c| !c.is_empty()).collect();
        }
        if let Some(o) = get("ORGANIZER") {
            e.organizer = Some(Person::new(o.param("CN").map(|c| c.trim_matches('"').to_string()).unwrap_or_default(), mailto(&o.value)));
        }
        for a in raw.props.iter().filter(|p| p.name == "ATTENDEE") {
            let kind = if a.param("CUTYPE").is_some_and(|c| c.eq_ignore_ascii_case("RESOURCE") || c.eq_ignore_ascii_case("ROOM")) {
                AttendeeKind::Resource
            } else if a.param("ROLE").is_some_and(|r| r.eq_ignore_ascii_case("OPT-PARTICIPANT") || r.eq_ignore_ascii_case("NON-PARTICIPANT")) {
                AttendeeKind::Optional
            } else {
                AttendeeKind::Required
            };
            e.attendees.push(Attendee {
                person: Person::new(a.param("CN").map(|c| c.trim_matches('"').to_string()).unwrap_or_default(), mailto(&a.value)),
                kind,
                status: partstat(a.param("PARTSTAT")),
            });
        }
        if let Some(t) = &raw.alarm_trigger
            && let Some(d) = parse_duration(t)
        {
            e.reminder = Some((-d / 60).max(0) as i32);
        }
        if let Some(u) = get("URL").or_else(|| get("X-MICROSOFT-SKYPETEAMSMEETINGURL")) {
            e.online_meeting_url = Some(u.value.trim().to_string());
        }
        if let Some(rid) = get("RECURRENCE-ID").and_then(parse_time) {
            overrides.push((uid, rid.0, e));
            continue;
        }
        if let Some(rr) = get("RRULE") {
            e.recurrence = parse_rrule(&rr.value, e.start_local(), tz);
        }
        for ex in raw.props.iter().filter(|p| p.name == "EXDATE") {
            for v in ex.value.split(',') {
                let p = Prop { name: "EXDATE".into(), params: ex.params.clone(), value: v.to_string() };
                if let Some((t, date_only, _)) = parse_time(&p) {
                    // Date-only EXDATEs name the day; match the series time of day.
                    let t = if date_only && !e.all_day {
                        let local = DateTime::from_timestamp(t, 0).unwrap_or_default().with_timezone(&tz).date_naive();
                        tz.from_local_datetime(&local.and_time(e.start_local().time())).earliest().map(|d| d.timestamp()).unwrap_or(t)
                    } else {
                        t
                    };
                    e.exception_mut(t).deleted = true;
                }
            }
        }
        masters.push(e);
    }
    for (uid, rid, o) in overrides {
        match masters.iter_mut().find(|m| m.uid == uid && m.recurrence.is_some()) {
            Some(m) => {
                let ex = m.exception_mut(rid);
                ex.deleted = o.cancelled;
                ex.subject = Some(o.subject.clone());
                ex.location = Some(o.location.clone());
                ex.body = Some(o.body.clone());
                ex.start = Some(o.start);
                ex.end = Some(o.end);
                ex.busy = Some(o.busy);
                ex.reminder = Some(o.reminder);
            }
            None => {
                // Only an occurrence was sent (e.g. reply to a single instance).
                let mut o = o;
                o.exceptions.push(Exception { original_start: rid, ..Default::default() });
                masters.push(o);
            }
        }
    }
    cal.events = masters;
    cal
}

// ------------------------------------------------------------------ write

fn fold(line: &str) -> String {
    let mut out = String::new();
    let mut len = 0;
    for c in line.chars() {
        let l = c.len_utf8();
        if len + l > 74 {
            out.push_str("\r\n ");
            len = 1;
        }
        out.push(c);
        len += l;
    }
    out.push_str("\r\n");
    out
}

fn fmt_utc(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().format("%Y%m%dT%H%M%SZ").to_string()
}

fn fmt_time(name: &str, ts: i64, all_day: bool, tz: Tz) -> String {
    let local = DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&tz);
    if all_day {
        format!("{name};VALUE=DATE:{}", local.format("%Y%m%d"))
    } else {
        format!("{name};TZID={}:{}", tz.name(), local.format("%Y%m%dT%H%M%S"))
    }
}

fn rrule(r: &Recurrence) -> String {
    let days = |mask: u8| -> Vec<&str> { (0..7).filter(|i| mask & (1 << i) != 0).map(|i| DAYS[i]).collect() };
    let mut s = String::from("RRULE:");
    let ord = |w: u32| if w >= 5 { -1 } else { w as i32 };
    match r.freq {
        Frequency::Daily if r.days != 0 => s.push_str(&format!("FREQ=WEEKLY;BYDAY={}", days(r.days).join(","))),
        Frequency::Daily => s.push_str("FREQ=DAILY"),
        Frequency::Weekly => s.push_str(&format!("FREQ=WEEKLY;BYDAY={}", days(r.days).join(","))),
        Frequency::Monthly => s.push_str(&format!("FREQ=MONTHLY;BYMONTHDAY={}", r.day_of_month)),
        Frequency::MonthlyNth => s.push_str(&format!("FREQ=MONTHLY;BYDAY={};BYSETPOS={}", days(r.days).join(","), ord(r.week_of_month))),
        Frequency::Yearly => s.push_str(&format!("FREQ=YEARLY;BYMONTH={};BYMONTHDAY={}", r.month, r.day_of_month)),
        Frequency::YearlyNth => s.push_str(&format!("FREQ=YEARLY;BYMONTH={};BYDAY={};BYSETPOS={}", r.month, days(r.days).join(","), ord(r.week_of_month))),
    }
    if r.interval > 1 && !(r.freq == Frequency::Daily && r.days != 0) {
        s.push_str(&format!(";INTERVAL={}", r.interval));
    }
    if let Some(c) = r.count {
        s.push_str(&format!(";COUNT={c}"));
    } else if let Some(u) = r.until {
        s.push_str(&format!(";UNTIL={}", fmt_utc(u)));
    }
    if r.freq == Frequency::Weekly {
        s.push_str(&format!(";WKST={}", DAYS[(r.first_day_of_week % 7) as usize]));
    }
    s
}

pub struct WriteOptions<'a> {
    pub method: &'a str,
    /// For METHOD:REPLY: the replying attendee and the answer.
    pub reply_from: Option<(&'a Person, Response)>,
    /// Write only this occurrence (original start) instead of the series.
    pub occurrence: Option<i64>,
}

/// iCalendar object for an event.
pub fn write(e: &Event, opts: &WriteOptions<'_>) -> String {
    let tz = e.zone();
    let mut s = String::new();
    s.push_str("BEGIN:VCALENDAR\r\nPRODID:-//MirraNET//Ondura Server//EN\r\nVERSION:2.0\r\n");
    s.push_str(&format!("METHOD:{}\r\n", opts.method));
    if !e.all_day {
        s.push_str(&tzinfo::vtimezone(tz));
    }
    let write_event = |s: &mut String, ev: &Event, rid: Option<i64>, with_rule: bool| {
        s.push_str("BEGIN:VEVENT\r\n");
        s.push_str(&fold(&format!("UID:{}", ev.uid)));
        if let Some(r) = rid {
            s.push_str(&fold(&fmt_time("RECURRENCE-ID", r, ev.all_day, tz)));
        }
        s.push_str(&format!("DTSTAMP:{}\r\n", fmt_utc(Utc::now().timestamp())));
        s.push_str(&format!("SEQUENCE:{}\r\n", ev.sequence));
        s.push_str(&fold(&fmt_time("DTSTART", ev.start, ev.all_day, tz)));
        s.push_str(&fold(&fmt_time("DTEND", ev.end, ev.all_day, tz)));
        s.push_str(&fold(&format!("SUMMARY:{}", escape(&ev.subject))));
        if !ev.location.is_empty() {
            s.push_str(&fold(&format!("LOCATION:{}", escape(&ev.location))));
        }
        if !ev.body.is_empty() && opts.method != "REPLY" {
            s.push_str(&fold(&format!("DESCRIPTION:{}", escape(&ev.body))));
        }
        if let Some(o) = &ev.organizer {
            let cn = if o.name.is_empty() { String::new() } else { format!(";CN=\"{}\"", o.name.replace('"', "'")) };
            s.push_str(&fold(&format!("ORGANIZER{cn}:mailto:{}", o.email)));
        }
        let attendees: Vec<Attendee> = match opts.reply_from {
            Some((who, answer)) => vec![Attendee { person: who.clone(), kind: AttendeeKind::Required, status: answer }],
            None => ev.attendees.clone(),
        };
        for a in &attendees {
            let role = match a.kind {
                AttendeeKind::Optional => "OPT-PARTICIPANT",
                _ => "REQ-PARTICIPANT",
            };
            let cutype = if a.kind == AttendeeKind::Resource { ";CUTYPE=RESOURCE" } else { "" };
            let cn = if a.person.name.is_empty() { String::new() } else { format!(";CN=\"{}\"", a.person.name.replace('"', "'")) };
            let rsvp = if opts.method == "REQUEST" { ";RSVP=TRUE" } else { "" };
            s.push_str(&fold(&format!("ATTENDEE;ROLE={role}{cutype};PARTSTAT={}{rsvp}{cn}:mailto:{}", partstat_str(a.status), a.person.email)));
        }
        if with_rule && let Some(r) = &ev.recurrence {
            s.push_str(&fold(&rrule(r)));
            let ex: Vec<String> = ev
                .exceptions
                .iter()
                .filter(|x| x.deleted)
                .map(|x| DateTime::from_timestamp(x.original_start, 0).unwrap_or_default().with_timezone(&tz))
                .map(|d| if ev.all_day { d.format("%Y%m%d").to_string() } else { d.format("%Y%m%dT%H%M%S").to_string() })
                .collect();
            if !ex.is_empty() {
                let head = if ev.all_day { "EXDATE;VALUE=DATE".to_string() } else { format!("EXDATE;TZID={}", tz.name()) };
                s.push_str(&fold(&format!("{head}:{}", ex.join(","))));
            }
        }
        if !ev.categories.is_empty() {
            s.push_str(&fold(&format!("CATEGORIES:{}", ev.categories.iter().map(|c| escape(c)).collect::<Vec<_>>().join(","))));
        }
        s.push_str(match ev.sensitivity {
            Sensitivity::Private => "CLASS:PRIVATE\r\n",
            Sensitivity::Confidential => "CLASS:CONFIDENTIAL\r\n",
            _ => "CLASS:PUBLIC\r\n",
        });
        s.push_str(if ev.busy == BusyStatus::Free { "TRANSP:TRANSPARENT\r\n" } else { "TRANSP:OPAQUE\r\n" });
        s.push_str(&format!(
            "X-MICROSOFT-CDO-BUSYSTATUS:{}\r\n",
            match ev.busy {
                BusyStatus::Free => "FREE",
                BusyStatus::Tentative => "TENTATIVE",
                BusyStatus::Oof => "OOF",
                BusyStatus::WorkingElsewhere => "WORKINGELSEWHERE",
                BusyStatus::Busy => "BUSY",
            }
        ));
        if opts.method == "CANCEL" || ev.cancelled {
            s.push_str("STATUS:CANCELLED\r\n");
        } else {
            s.push_str("STATUS:CONFIRMED\r\n");
        }
        if let Some(url) = &ev.online_meeting_url {
            s.push_str(&fold(&format!("URL:{url}")));
        }
        if let Some(m) = ev.reminder
            && opts.method != "REPLY"
        {
            s.push_str(&format!("BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\nTRIGGER:-PT{m}M\r\nEND:VALARM\r\n"));
        }
        s.push_str("END:VEVENT\r\n");
    };
    match opts.occurrence {
        Some(original) => {
            let occ = e.occurrences(original - 1, original + 1).into_iter().find(|o| o.original_start == original);
            let mut one = e.clone();
            one.recurrence = None;
            one.exceptions.clear();
            if let Some(o) = occ {
                one.start = o.start;
                one.end = o.end;
                if let Some(x) = o.exception {
                    one.subject = x.subject.unwrap_or(one.subject);
                    one.location = x.location.unwrap_or(one.location);
                }
            } else {
                let d = e.end - e.start;
                one.start = original;
                one.end = original + d;
            }
            write_event(&mut s, &one, Some(original), false);
        }
        None => {
            write_event(&mut s, e, None, true);
            if e.recurrence.is_some() {
                for x in e.exceptions.iter().filter(|x| !x.deleted) {
                    let mut o = e.clone();
                    o.recurrence = None;
                    o.exceptions.clear();
                    o.start = x.start.unwrap_or(x.original_start);
                    o.end = x.end.unwrap_or(o.start + (e.end - e.start));
                    if let Some(v) = &x.subject {
                        o.subject = v.clone();
                    }
                    if let Some(v) = &x.location {
                        o.location = v.clone();
                    }
                    if let Some(v) = &x.body {
                        o.body = v.clone();
                    }
                    if let Some(v) = x.busy {
                        o.busy = v;
                    }
                    write_event(&mut s, &o, Some(x.original_start), false);
                }
            }
        }
    }
    s.push_str("END:VCALENDAR\r\n");
    s
}

/// Text body for invitation mails.
pub fn summary_text(e: &Event) -> String {
    let tz = e.zone();
    let start = DateTime::from_timestamp(e.start, 0).unwrap_or_default().with_timezone(&tz);
    let end = DateTime::from_timestamp(e.end, 0).unwrap_or_default().with_timezone(&tz);
    let when = if e.all_day {
        let last = end - chrono::Duration::days(1);
        if last.date_naive() > start.date_naive() {
            format!("{} – {}", start.format("%a %d.%m.%Y"), last.format("%a %d.%m.%Y"))
        } else {
            start.format("%a %d.%m.%Y").to_string()
        }
    } else {
        format!("{} – {} ({})", start.format("%a %d.%m.%Y %H:%M"), end.format("%H:%M"), tz.name())
    };
    let mut s = format!("{}\n\nWhen: {when}\n", e.subject);
    if !e.location.is_empty() {
        s.push_str(&format!("Where: {}\n", e.location));
    }
    if let Some(o) = &e.organizer {
        s.push_str(&format!("Organizer: {}\n", o.display()));
    }
    if !e.body.is_empty() {
        s.push_str(&format!("\n{}\n", e.body));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nPRODID:test\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:W. Europe Standard Time\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:abc-123\r\nSUMMARY:Jour fixe\\, Team\r\nDTSTART;TZID=W. Europe Standard Time:20261102T090000\r\nDTEND;TZID=W. Europe Standard Time:20261102T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=10\r\nEXDATE;TZID=W. Europe Standard Time:20261109T090000\r\nORGANIZER;CN=\"Max Becker\":mailto:Max@Example.com\r\nATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;CN=Niklas:mailto:niklas@mirranet.de\r\nATTENDEE;ROLE=OPT-PARTICIPANT;PARTSTAT=ACCEPTED:mailto:anna@mirranet.de\r\nBEGIN:VALARM\r\nTRIGGER:-PT15M\r\nACTION:DISPLAY\r\nEND:VALARM\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:abc-123\r\nRECURRENCE-ID;TZID=W. Europe Standard Time:20261116T090000\r\nSUMMARY:Jour fixe (verschoben)\r\nDTSTART;TZID=W. Europe Standard Time:20261116T140000\r\nDTEND;TZID=W. Europe Standard Time:20261116T150000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn parses_invitation() {
        let cal = parse(INVITE);
        assert_eq!(cal.method, "REQUEST");
        assert_eq!(cal.events.len(), 1);
        let e = &cal.events[0];
        assert_eq!(e.subject, "Jour fixe, Team");
        assert_eq!(e.tz, "Europe/Berlin");
        assert_eq!(e.reminder, Some(15));
        assert_eq!(e.organizer.as_ref().unwrap().email, "max@example.com");
        assert_eq!(e.attendees[1].kind, AttendeeKind::Optional);
        let r = e.recurrence.as_ref().unwrap();
        assert_eq!((r.freq, r.count), (Frequency::Weekly, Some(10)));
        assert_eq!(e.exceptions.len(), 2);
        let occ = e.occurrences(e.start, e.start + 30 * 86_400);
        assert_eq!(occ.len(), 4); // 2.11., 16.11. (moved), 23.11., 30.11. — 9.11. deleted
        assert_eq!(occ[1].exception.as_ref().unwrap().subject.as_deref(), Some("Jour fixe (verschoben)"));
    }

    #[test]
    fn writes_and_reads_back() {
        let e = parse(INVITE).events.remove(0);
        let text = write(&e, &WriteOptions { method: "REQUEST", reply_from: None, occurrence: None });
        assert!(text.contains("RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=10;WKST=MO"));
        assert!(text.contains("BEGIN:VTIMEZONE"));
        let back = parse(&text).events.remove(0);
        assert_eq!(back.start, e.start);
        assert_eq!(back.exceptions.len(), 2);
        assert_eq!(back.occurrences(e.start, e.start + 30 * 86_400).len(), 4);

        let me = Person::new("Niklas", "niklas@mirranet.de");
        let reply = write(&e, &WriteOptions { method: "REPLY", reply_from: Some((&me, Response::Accepted)), occurrence: None });
        let r = parse(&reply);
        assert_eq!(r.method, "REPLY");
        assert_eq!(r.events[0].attendees.len(), 1);
        assert_eq!(r.events[0].attendees[0].status, Response::Accepted);
    }

    #[test]
    fn all_day_and_durations() {
        let c = parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:x\nDTSTART;VALUE=DATE:20261224\nDURATION:P2D\nSUMMARY:Weihnachten\nEND:VEVENT\nEND:VCALENDAR\n");
        let e = &c.events[0];
        assert!(e.all_day);
        assert_eq!(e.end - e.start, 2 * 86_400);
        assert_eq!(parse_duration("-PT1H30M"), Some(-5400));
    }
}
