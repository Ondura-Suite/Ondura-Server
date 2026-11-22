//! Reading items from EWS requests (CreateItem, UpdateItem).

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone};
use chrono_tz::Tz;

use super::write::{parse_days_of_week, parse_index, parse_month};
use crate::mime;
use crate::model::{Address, Attendee, AttendeeKind, BusyStatus, Contact, Event, Frequency, Importance, Person, Recurrence, Response, Sensitivity, Task, flags};
use crate::tzinfo;
use crate::xml::Node;

/// Date-time: with offset/Z, or local in `tz`.
pub fn parse_time(s: &str, tz: Tz) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, fmt) {
            return tz.from_local_datetime(&d).earliest().map(|x| x.timestamp());
        }
    }
    let date = s.split(['+', 'Z']).next().unwrap_or(s);
    NaiveDate::parse_from_str(date.trim_end_matches('-'), "%Y-%m-%d")
        .ok()
        .and_then(|d| tz.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest())
        .map(|x| x.timestamp())
}

pub fn persons(n: Option<&Node>) -> Vec<Person> {
    let Some(n) = n else { return vec![] };
    let mut out = Vec::new();
    let mut boxes = Vec::new();
    n.find_all("Mailbox", &mut boxes);
    for b in boxes {
        let email = b.text_of("EmailAddress").unwrap_or_default().to_string();
        if email.is_empty() {
            continue;
        }
        out.push(Person::new(b.text_of("Name").unwrap_or_default(), email));
    }
    out
}

fn body(n: &Node) -> Option<(String, Option<String>)> {
    let b = n.child("Body")?;
    if b.attr("BodyType").is_some_and(|t| t.eq_ignore_ascii_case("HTML")) {
        Some((mime::html_to_text(&b.text), Some(b.text.clone())))
    } else {
        Some((b.text.clone(), None))
    }
}

fn categories(n: &Node) -> Option<Vec<String>> {
    n.child("Categories").map(|c| c.children_named("String").map(|s| s.text.trim().to_string()).filter(|s| !s.is_empty()).collect())
}

fn is_true(v: Option<&str>) -> Option<bool> {
    v.map(|v| v.eq_ignore_ascii_case("true") || v == "1")
}

/// Zone named by StartTimeZone/MeetingTimeZone/TimeZone of an item.
pub fn item_zone(n: &Node) -> Option<Tz> {
    for name in ["StartTimeZone", "MeetingTimeZone"] {
        if let Some(z) = n.child(name) {
            if let Some(tz) = z.attr("Id").and_then(tzinfo::from_windows_name).or_else(|| z.attr("TimeZoneName").and_then(tzinfo::from_windows_name)) {
                return Some(tz);
            }
        }
    }
    n.text_of("TimeZone").and_then(tzinfo::from_windows_name)
}

pub fn parse_recurrence(n: &Node, tz: Tz, start_local: NaiveDateTime) -> Option<Recurrence> {
    let mut r = Recurrence { freq: Frequency::Daily, interval: 1, days: 0, day_of_month: 0, week_of_month: 0, month: 0, until: None, count: None, first_day_of_week: 0 };
    let mut found = false;
    for p in &n.children {
        let interval = p.text_of("Interval").and_then(|i| i.parse().ok()).unwrap_or(1);
        match p.name.as_str() {
            "DailyRecurrence" => {
                r.freq = Frequency::Daily;
                r.interval = interval;
            }
            "WeeklyRecurrence" => {
                r.freq = Frequency::Weekly;
                r.interval = interval;
                r.days = parse_days_of_week(p.text_of("DaysOfWeek").unwrap_or_default());
                r.first_day_of_week = p.text_of("FirstDayOfWeek").map(|d| parse_days_of_week(d).trailing_zeros() as u8).unwrap_or(0);
            }
            "AbsoluteMonthlyRecurrence" => {
                r.freq = Frequency::Monthly;
                r.interval = interval;
                r.day_of_month = p.text_of("DayOfMonth").and_then(|d| d.parse().ok()).unwrap_or(0);
            }
            "RelativeMonthlyRecurrence" => {
                r.freq = Frequency::MonthlyNth;
                r.interval = interval;
                r.days = parse_days_of_week(p.text_of("DaysOfWeek").unwrap_or_default());
                r.week_of_month = parse_index(p.text_of("DayOfWeekIndex").unwrap_or("First"));
            }
            "AbsoluteYearlyRecurrence" => {
                r.freq = Frequency::Yearly;
                r.day_of_month = p.text_of("DayOfMonth").and_then(|d| d.parse().ok()).unwrap_or(0);
                r.month = parse_month(p.text_of("Month").unwrap_or("January"));
            }
            "RelativeYearlyRecurrence" => {
                r.freq = Frequency::YearlyNth;
                r.days = parse_days_of_week(p.text_of("DaysOfWeek").unwrap_or_default());
                r.week_of_month = parse_index(p.text_of("DayOfWeekIndex").unwrap_or("First"));
                r.month = parse_month(p.text_of("Month").unwrap_or("January"));
            }
            "NumberedRecurrence" => r.count = p.text_of("NumberOfOccurrences").and_then(|c| c.parse().ok()),
            "EndDateRecurrence" => r.until = p.text_of("EndDate").and_then(|d| parse_time(d, tz)).map(|t| t + 86_399),
            "NoEndRecurrence" => {}
            _ => continue,
        }
        if p.name.ends_with("Recurrence") && !matches!(p.name.as_str(), "NumberedRecurrence" | "EndDateRecurrence" | "NoEndRecurrence") {
            found = true;
        }
    }
    if !found {
        return None;
    }
    r.normalize(start_local);
    Some(r)
}

fn attendees(n: Option<&Node>, kind: AttendeeKind, old: &[Attendee]) -> Vec<Attendee> {
    let Some(n) = n else { return vec![] };
    n.children_named("Attendee")
        .filter_map(|a| {
            let p = persons(Some(a)).into_iter().next()?;
            let prev = old.iter().find(|o| o.person.email.eq_ignore_ascii_case(&p.email)).map(|o| o.status);
            Some(Attendee { person: p, kind, status: prev.unwrap_or(Response::NotResponded) })
        })
        .collect()
}

/// Apply the fields present in a `CalendarItem` element.
pub fn apply_event(n: &Node, mut e: Event, default_tz: Tz) -> Event {
    let tz = item_zone(n).unwrap_or_else(|| if e.tz.is_empty() { default_tz } else { e.zone() });
    e.tz = tz.name().to_string();
    if let Some(v) = n.raw_text_of("Subject") {
        e.subject = v.to_string();
    }
    if let Some((text, html)) = body(n) {
        e.body = text;
        e.body_html = html;
    }
    if let Some(c) = categories(n) {
        e.categories = c;
    }
    if let Some(v) = n.text_of("Sensitivity") {
        e.sensitivity = Sensitivity::from_ews(v);
    }
    let reminder_set = is_true(n.text_of("ReminderIsSet"));
    let minutes: Option<i32> = n.text_of("ReminderMinutesBeforeStart").and_then(|m| m.parse().ok());
    match (reminder_set, minutes) {
        (Some(false), _) => e.reminder = None,
        (Some(true), m) => e.reminder = Some(m.or(e.reminder).unwrap_or(15)),
        (None, Some(m)) if e.reminder.is_some() => e.reminder = Some(m),
        _ => {}
    }
    if let Some(v) = n.text_of("Start").and_then(|s| parse_time(s, tz)) {
        e.start = v;
    }
    if let Some(v) = n.text_of("End").and_then(|s| parse_time(s, tz)) {
        e.end = v;
    }
    if e.end < e.start {
        e.end = e.start;
    }
    if let Some(v) = is_true(n.text_of("IsAllDayEvent")) {
        e.all_day = v;
    }
    if let Some(v) = n.text_of("LegacyFreeBusyStatus") {
        e.busy = BusyStatus::from_ews(v);
    }
    if let Some(v) = n.raw_text_of("Location") {
        e.location = v.trim().to_string();
    }
    if let Some(v) = n.text_of("UID").filter(|u| !u.is_empty()) {
        e.uid = v.to_string();
    }
    if n.child("RequiredAttendees").is_some() || n.child("OptionalAttendees").is_some() || n.child("Resources").is_some() {
        let old = e.attendees.clone();
        let mut keep: Vec<Attendee> = Vec::new();
        for (name, kind) in [("RequiredAttendees", AttendeeKind::Required), ("OptionalAttendees", AttendeeKind::Optional), ("Resources", AttendeeKind::Resource)] {
            match n.child(name) {
                Some(list) => keep.extend(attendees(Some(list), kind, &old)),
                None => keep.extend(old.iter().filter(|a| a.kind == kind).cloned()),
            }
        }
        e.attendees = keep;
    }
    if let Some(r) = n.child("Recurrence") {
        e.recurrence = parse_recurrence(r, tz, e.start_local());
    }
    if e.uid.is_empty() {
        e.uid = uuid::Uuid::new_v4().to_string().to_uppercase();
    }
    e
}

fn merge_entries(n: &Node, list: &str, mut f: impl FnMut(&str, &Node)) {
    if let Some(l) = n.child(list) {
        for entry in l.children_named("Entry") {
            f(entry.attr("Key").unwrap_or_default(), entry);
        }
    }
}

fn set_text(field: &mut String, n: &Node, name: &str) {
    if let Some(v) = n.raw_text_of(name) {
        *field = v.trim().to_string();
    }
}

pub fn apply_contact(n: &Node, mut c: Contact) -> Contact {
    set_text(&mut c.first_name, n, "GivenName");
    set_text(&mut c.last_name, n, "Surname");
    set_text(&mut c.middle_name, n, "MiddleName");
    set_text(&mut c.nickname, n, "Nickname");
    set_text(&mut c.display_name, n, "DisplayName");
    set_text(&mut c.file_as, n, "FileAs");
    set_text(&mut c.company, n, "CompanyName");
    set_text(&mut c.job_title, n, "JobTitle");
    set_text(&mut c.department, n, "Department");
    set_text(&mut c.office, n, "OfficeLocation");
    set_text(&mut c.manager, n, "Manager");
    set_text(&mut c.assistant, n, "AssistantName");
    set_text(&mut c.spouse, n, "SpouseName");
    set_text(&mut c.suffix, n, "Generation");
    set_text(&mut c.web_page, n, "BusinessHomePage");
    if let Some((text, _)) = body(n) {
        c.notes = text;
    }
    if let Some(cats) = categories(n) {
        c.categories = cats;
    }
    if let Some(b) = n.text_of("Birthday") {
        c.birthday = Some(b.get(..10).unwrap_or(b).to_string());
    }
    if let Some(b) = n.text_of("WeddingAnniversary") {
        c.anniversary = Some(b.get(..10).unwrap_or(b).to_string());
    }
    if let Some(ch) = n.child("Children") {
        c.children = ch.children_named("String").map(|s| s.text.trim().to_string()).collect();
    }
    merge_entries(n, "EmailAddresses", |key, e| {
        let idx = key.trim_start_matches("EmailAddress").parse::<usize>().unwrap_or(1).clamp(1, 3) - 1;
        let v = e.text.trim().trim_start_matches("SMTP:").to_string();
        while c.emails.len() <= idx {
            c.emails.push(String::new());
        }
        c.emails[idx] = v;
    });
    c.emails.retain(|e| !e.is_empty());
    merge_entries(n, "ImAddresses", |key, e| {
        let idx = key.trim_start_matches("ImAddress").parse::<usize>().unwrap_or(1).clamp(1, 3) - 1;
        while c.im.len() <= idx {
            c.im.push(String::new());
        }
        c.im[idx] = e.text.trim().to_string();
    });
    c.im.retain(|e| !e.is_empty());
    merge_entries(n, "PhoneNumbers", |key, e| {
        let v = e.text.trim().to_string();
        let field = match key {
            "AssistantPhone" => &mut c.assistant_phone,
            "BusinessFax" => &mut c.business_fax,
            "BusinessPhone" => &mut c.business_phone,
            "BusinessPhone2" => &mut c.business_phone2,
            "CarPhone" => &mut c.car_phone,
            "CompanyMainPhone" => &mut c.company_main_phone,
            "HomeFax" => &mut c.home_fax,
            "HomePhone" => &mut c.home_phone,
            "HomePhone2" => &mut c.home_phone2,
            "MobilePhone" => &mut c.mobile_phone,
            "Pager" => &mut c.pager,
            "RadioPhone" => &mut c.radio_phone,
            _ => return,
        };
        *field = v;
    });
    merge_entries(n, "PhysicalAddresses", |key, e| {
        let a: &mut Address = match key {
            "Business" => &mut c.business_address,
            "Home" => &mut c.home_address,
            _ => &mut c.other_address,
        };
        set_text(&mut a.street, e, "Street");
        set_text(&mut a.city, e, "City");
        set_text(&mut a.state, e, "State");
        set_text(&mut a.country, e, "CountryOrRegion");
        set_text(&mut a.postal_code, e, "PostalCode");
    });
    if c.display_name.is_empty() {
        c.display_name = c.effective_name();
    }
    c
}

pub fn apply_task(n: &Node, mut t: Task, tz: Tz) -> Task {
    if let Some(v) = n.raw_text_of("Subject") {
        t.subject = v.to_string();
    }
    if let Some((text, _)) = body(n) {
        t.body = text;
    }
    if let Some(c) = categories(n) {
        t.categories = c;
    }
    if let Some(v) = n.text_of("Importance") {
        t.importance = Importance::from_ews(v);
    }
    if let Some(v) = n.text_of("Sensitivity") {
        t.sensitivity = Sensitivity::from_ews(v);
    }
    if let Some(v) = n.text_of("StartDate") {
        t.start = parse_time(v, tz);
    }
    if let Some(v) = n.text_of("DueDate") {
        t.due = parse_time(v, tz);
    }
    if let Some(v) = n.text_of("PercentComplete").and_then(|p| p.parse::<f64>().ok()) {
        t.percent_complete = v.clamp(0.0, 100.0) as u8;
        t.complete = t.percent_complete >= 100;
    }
    if let Some(v) = n.text_of("Status") {
        t.complete = v == "Completed";
        if t.complete {
            t.percent_complete = 100;
        }
    }
    if let Some(v) = is_true(n.text_of("IsComplete")) {
        t.complete = v;
    }
    if let Some(v) = n.text_of("CompleteDate") {
        t.completed_at = parse_time(v, tz);
    }
    if t.complete && t.completed_at.is_none() {
        t.completed_at = Some(chrono::Utc::now().timestamp());
    }
    match (is_true(n.text_of("ReminderIsSet")), n.text_of("ReminderDueBy").and_then(|v| parse_time(v, tz))) {
        (Some(false), _) => t.reminder = None,
        (_, Some(at)) => t.reminder = Some(at),
        (Some(true), None) => t.reminder = t.reminder.or(t.due),
        _ => {}
    }
    t
}

/// Clear a field named by a FieldURI (DeleteItemField).
pub fn delete_event_field(e: &mut Event, uri: &str) {
    match uri {
        "item:Body" => {
            e.body.clear();
            e.body_html = None;
        }
        "item:Categories" => e.categories.clear(),
        "calendar:Location" => e.location.clear(),
        "calendar:Recurrence" => {
            e.recurrence = None;
            e.exceptions.clear();
        }
        "calendar:RequiredAttendees" => e.attendees.retain(|a| a.kind != AttendeeKind::Required),
        "calendar:OptionalAttendees" => e.attendees.retain(|a| a.kind != AttendeeKind::Optional),
        "calendar:Resources" => e.attendees.retain(|a| a.kind != AttendeeKind::Resource),
        "item:ReminderMinutesBeforeStart" | "item:ReminderIsSet" => e.reminder = None,
        _ => {}
    }
}

pub fn delete_contact_field(c: &mut Contact, uri: &str, index: Option<&str>) {
    match (uri, index) {
        ("contacts:EmailAddress", Some(i)) => {
            let idx = i.trim_start_matches("EmailAddress").parse::<usize>().unwrap_or(1).saturating_sub(1);
            if idx < c.emails.len() {
                c.emails.remove(idx);
            }
        }
        ("contacts:PhoneNumber", Some(key)) => {
            let empty = Contact::default();
            let mut tmp = c.clone();
            let n = Node {
                name: "Contact".into(),
                children: vec![Node { name: "PhoneNumbers".into(), children: vec![Node { name: "Entry".into(), attrs: vec![("Key".into(), key.into())], ..Default::default() }], ..Default::default() }],
                ..Default::default()
            };
            tmp = apply_contact(&n, tmp);
            *c = tmp;
            let _ = empty;
        }
        ("contacts:PhysicalAddress:Street" | "contacts:PhysicalAddress:City" | "contacts:PhysicalAddress:PostalCode" | "contacts:PhysicalAddress:CountryOrRegion" | "contacts:PhysicalAddress:State", Some(key)) => {
            let a = match key {
                "Business" => &mut c.business_address,
                "Home" => &mut c.home_address,
                _ => &mut c.other_address,
            };
            match uri.rsplit(':').next() {
                Some("Street") => a.street.clear(),
                Some("City") => a.city.clear(),
                Some("PostalCode") => a.postal_code.clear(),
                Some("CountryOrRegion") => a.country.clear(),
                Some("State") => a.state.clear(),
                _ => {}
            }
        }
        ("contacts:GivenName", _) => c.first_name.clear(),
        ("contacts:Surname", _) => c.last_name.clear(),
        ("contacts:MiddleName", _) => c.middle_name.clear(),
        ("contacts:CompanyName", _) => c.company.clear(),
        ("contacts:JobTitle", _) => c.job_title.clear(),
        ("contacts:Department", _) => c.department.clear(),
        ("contacts:OfficeLocation", _) => c.office.clear(),
        ("contacts:Birthday", _) => c.birthday = None,
        ("contacts:WeddingAnniversary", _) => c.anniversary = None,
        ("contacts:Nickname", _) => c.nickname.clear(),
        ("contacts:BusinessHomePage", _) => c.web_page.clear(),
        ("item:Body", _) => c.notes.clear(),
        ("item:Categories", _) => c.categories.clear(),
        _ => {}
    }
}

/// Flag/read/category changes of a `Message` element: (set, clear, categories).
pub fn message_flags(n: &Node) -> (i64, i64, Option<Vec<String>>) {
    let (mut set, mut clear) = (0, 0);
    match is_true(n.text_of("IsRead")) {
        Some(true) => set |= flags::READ,
        Some(false) => clear |= flags::READ,
        None => {}
    }
    if let Some(f) = n.child("Flag") {
        match f.text_of("FlagStatus") {
            Some("Flagged") => {
                set |= flags::FLAGGED;
                clear |= flags::COMPLETE;
            }
            Some("Complete") => {
                set |= flags::COMPLETE;
                clear |= flags::FLAGGED;
            }
            _ => clear |= flags::FLAGGED | flags::COMPLETE,
        }
    }
    (set, clear, categories(n))
}

/// A message built from item fields (CreateItem without MimeContent).
pub fn outgoing_from_fields(n: &Node, from: Person) -> mime::Outgoing {
    let (text, html) = body(n).unwrap_or_default();
    let mut headers = Vec::new();
    match n.text_of("Importance") {
        Some("High") => headers.push(("Importance".to_string(), "high".to_string())),
        Some("Low") => headers.push(("Importance".to_string(), "low".to_string())),
        _ => {}
    }
    let bcc = persons(n.child("BccRecipients"));
    if !bcc.is_empty() {
        headers.push(("Bcc".to_string(), mime::format_list(&bcc)));
    }
    mime::Outgoing {
        from: persons(n.child("From")).into_iter().next().unwrap_or(from),
        to: persons(n.child("ToRecipients")),
        cc: persons(n.child("CcRecipients")),
        subject: n.raw_text_of("Subject").unwrap_or_default().to_string(),
        text,
        html,
        headers,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let berlin = chrono_tz::Europe::Berlin;
        assert_eq!(parse_time("2026-11-16T08:00:00Z", berlin), parse_time("2026-11-16T09:00:00", berlin));
        assert_eq!(parse_time("2026-11-16T09:00:00+01:00", chrono_tz::UTC), parse_time("2026-11-16T08:00:00Z", berlin));
        assert!(parse_time("2026-11-16", berlin).is_some());
    }

    #[test]
    fn recurrence_from_xml() {
        let x = Node::parse("<t:Recurrence><t:WeeklyRecurrence><t:Interval>2</t:Interval><t:DaysOfWeek>Monday Thursday</t:DaysOfWeek></t:WeeklyRecurrence><t:NumberedRecurrence><t:StartDate>2026-11-16</t:StartDate><t:NumberOfOccurrences>5</t:NumberOfOccurrences></t:NumberedRecurrence></t:Recurrence>").unwrap();
        let start = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap().and_hms_opt(9, 0, 0).unwrap();
        let r = parse_recurrence(&x.children[0], chrono_tz::Europe::Berlin, start).unwrap();
        assert_eq!((r.freq, r.interval, r.days, r.count), (Frequency::Weekly, 2, 2 | 16, Some(5)));
    }
}
