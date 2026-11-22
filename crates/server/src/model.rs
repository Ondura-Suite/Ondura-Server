//! Calendar, contact and task items as stored by the server, plus the
//! expansion of recurring events. All protocol front ends (ActiveSync, EWS)
//! convert from and to these types.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Kind of a stored item; equals the `kind` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    Mail,
    Event,
    Contact,
    Task,
    Note,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Mail => "mail",
            ItemKind::Event => "event",
            ItemKind::Contact => "contact",
            ItemKind::Task => "task",
            ItemKind::Note => "note",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "mail" => ItemKind::Mail,
            "event" => ItemKind::Event,
            "contact" => ItemKind::Contact,
            "task" => ItemKind::Task,
            "note" => ItemKind::Note,
            _ => return None,
        })
    }
}

/// Class of a folder; equals the `class` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderClass {
    Root,
    Mail,
    Calendar,
    Contacts,
    Tasks,
    Notes,
}

impl FolderClass {
    pub fn as_str(self) -> &'static str {
        match self {
            FolderClass::Root => "root",
            FolderClass::Mail => "mail",
            FolderClass::Calendar => "calendar",
            FolderClass::Contacts => "contacts",
            FolderClass::Tasks => "tasks",
            FolderClass::Notes => "notes",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "root" => FolderClass::Root,
            "mail" => FolderClass::Mail,
            "calendar" => FolderClass::Calendar,
            "contacts" => FolderClass::Contacts,
            "tasks" => FolderClass::Tasks,
            "notes" => FolderClass::Notes,
            _ => return None,
        })
    }


    /// Exchange folder class (`IPF.Note`, …).
    pub fn ipf(self) -> &'static str {
        match self {
            FolderClass::Root => "",
            FolderClass::Mail => "IPF.Note",
            FolderClass::Calendar => "IPF.Appointment",
            FolderClass::Contacts => "IPF.Contact",
            FolderClass::Tasks => "IPF.Task",
            FolderClass::Notes => "IPF.StickyNote",
        }
    }

    pub fn from_ipf(s: &str) -> FolderClass {
        if s.starts_with("IPF.Appointment") {
            FolderClass::Calendar
        } else if s.starts_with("IPF.Contact") {
            FolderClass::Contacts
        } else if s.starts_with("IPF.Task") {
            FolderClass::Tasks
        } else if s.starts_with("IPF.StickyNote") {
            FolderClass::Notes
        } else {
            FolderClass::Mail
        }
    }
}

/// Well-known folders every mailbox has. The names are the EWS
/// distinguished folder ids.
pub const DEFAULT_FOLDERS: &[(&str, FolderClass, &str)] = &[
    ("inbox", FolderClass::Mail, "Inbox"),
    ("drafts", FolderClass::Mail, "Drafts"),
    ("sentitems", FolderClass::Mail, "Sent Items"),
    ("deleteditems", FolderClass::Mail, "Deleted Items"),
    ("junkemail", FolderClass::Mail, "Junk Email"),
    ("outbox", FolderClass::Mail, "Outbox"),
    ("archive", FolderClass::Mail, "Archive"),
    ("calendar", FolderClass::Calendar, "Calendar"),
    ("contacts", FolderClass::Contacts, "Contacts"),
    ("tasks", FolderClass::Tasks, "Tasks"),
    ("notes", FolderClass::Notes, "Notes"),
];

// ------------------------------------------------------------------ people

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    pub email: String,
}

impl Person {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Person { name: name.into(), email: email.into() }
    }

    pub fn display(&self) -> &str {
        if self.name.is_empty() { &self.email } else { &self.name }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttendeeKind {
    #[default]
    Required,
    Optional,
    Resource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Response {
    #[default]
    None,
    Organizer,
    Tentative,
    Accepted,
    Declined,
    NotResponded,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attendee {
    #[serde(flatten)]
    pub person: Person,
    #[serde(default)]
    pub kind: AttendeeKind,
    #[serde(default)]
    pub status: Response,
}

// ---------------------------------------------------------------- calendar

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BusyStatus {
    Free,
    Tentative,
    #[default]
    Busy,
    Oof,
    WorkingElsewhere,
}

impl BusyStatus {
    pub fn eas(self) -> u8 {
        match self {
            BusyStatus::Free => 0,
            BusyStatus::Tentative => 1,
            BusyStatus::Busy => 2,
            BusyStatus::Oof => 3,
            BusyStatus::WorkingElsewhere => 4,
        }
    }

    pub fn from_eas(v: &str) -> Self {
        match v.trim() {
            "0" => BusyStatus::Free,
            "1" => BusyStatus::Tentative,
            "3" => BusyStatus::Oof,
            "4" => BusyStatus::WorkingElsewhere,
            _ => BusyStatus::Busy,
        }
    }

    pub fn ews(self) -> &'static str {
        match self {
            BusyStatus::Free => "Free",
            BusyStatus::Tentative => "Tentative",
            BusyStatus::Busy => "Busy",
            BusyStatus::Oof => "OOF",
            BusyStatus::WorkingElsewhere => "WorkingElsewhere",
        }
    }

    pub fn from_ews(v: &str) -> Self {
        match v {
            "Free" => BusyStatus::Free,
            "Tentative" => BusyStatus::Tentative,
            "OOF" => BusyStatus::Oof,
            "WorkingElsewhere" => BusyStatus::WorkingElsewhere,
            _ => BusyStatus::Busy,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    #[default]
    Normal,
    Personal,
    Private,
    Confidential,
}

impl Sensitivity {
    pub fn eas(self) -> u8 {
        self as u8
    }

    pub fn from_eas(v: &str) -> Self {
        match v.trim() {
            "1" => Sensitivity::Personal,
            "2" => Sensitivity::Private,
            "3" => Sensitivity::Confidential,
            _ => Sensitivity::Normal,
        }
    }

    pub fn ews(self) -> &'static str {
        match self {
            Sensitivity::Normal => "Normal",
            Sensitivity::Personal => "Personal",
            Sensitivity::Private => "Private",
            Sensitivity::Confidential => "Confidential",
        }
    }

    pub fn from_ews(v: &str) -> Self {
        match v {
            "Personal" => Sensitivity::Personal,
            "Private" => Sensitivity::Private,
            "Confidential" => Sensitivity::Confidential,
            _ => Sensitivity::Normal,
        }
    }
}

/// Recurrence patterns as defined by Exchange (MS-ASCAL `Recurrence/Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frequency {
    Daily,
    Weekly,
    /// Day `day_of_month` of every `interval` months.
    Monthly,
    /// The `week_of_month`th `days` of every `interval` months (5 = last).
    MonthlyNth,
    /// `day_of_month` of `month` every `interval` years.
    Yearly,
    /// The `week_of_month`th `days` of `month` every `interval` years.
    YearlyNth,
}

impl Frequency {
    pub fn eas(self) -> u8 {
        match self {
            Frequency::Daily => 0,
            Frequency::Weekly => 1,
            Frequency::Monthly => 2,
            Frequency::MonthlyNth => 3,
            Frequency::Yearly => 5,
            Frequency::YearlyNth => 6,
        }
    }

    pub fn from_eas(v: &str) -> Option<Self> {
        Some(match v.trim() {
            "0" => Frequency::Daily,
            "1" => Frequency::Weekly,
            "2" => Frequency::Monthly,
            "3" => Frequency::MonthlyNth,
            "5" => Frequency::Yearly,
            "6" => Frequency::YearlyNth,
            _ => return None,
        })
    }
}

/// Day-of-week bit mask as used by ActiveSync: Sunday = 1 … Saturday = 64.
pub fn day_bit(d: Weekday) -> u8 {
    1 << d.num_days_from_sunday()
}

pub const WEEKDAYS: [Weekday; 7] = [Weekday::Sun, Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recurrence {
    pub freq: Frequency,
    #[serde(default = "one")]
    pub interval: u32,
    /// ActiveSync day mask (Sunday = 1 … Saturday = 64).
    #[serde(default)]
    pub days: u8,
    #[serde(default)]
    pub day_of_month: u32,
    /// 1–4, 5 = last.
    #[serde(default)]
    pub week_of_month: u32,
    /// 1–12.
    #[serde(default)]
    pub month: u32,
    /// Last day on which an occurrence may start (inclusive), as UTC timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// 0 = Sunday.
    #[serde(default)]
    pub first_day_of_week: u8,
}

fn one() -> u32 {
    1
}

impl Recurrence {
    /// Fill in pattern fields that clients may omit, from the series start.
    pub fn normalize(&mut self, start_local: NaiveDateTime) {
        self.interval = self.interval.max(1);
        let d = start_local.date();
        match self.freq {
            Frequency::Daily => {}
            Frequency::Weekly => {
                if self.days == 0 {
                    self.days = day_bit(d.weekday());
                }
            }
            Frequency::Monthly => {
                if self.day_of_month == 0 {
                    self.day_of_month = d.day();
                }
            }
            Frequency::MonthlyNth => {
                if self.days == 0 {
                    self.days = day_bit(d.weekday());
                }
                if self.week_of_month == 0 {
                    self.week_of_month = (d.day() - 1) / 7 + 1;
                }
            }
            Frequency::Yearly => {
                if self.day_of_month == 0 {
                    self.day_of_month = d.day();
                }
                if self.month == 0 {
                    self.month = d.month();
                }
            }
            Frequency::YearlyNth => {
                if self.days == 0 {
                    self.days = day_bit(d.weekday());
                }
                if self.week_of_month == 0 {
                    self.week_of_month = (d.day() - 1) / 7 + 1;
                }
                if self.month == 0 {
                    self.month = d.month();
                }
            }
        }
    }
}

/// A changed or deleted occurrence of a series.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exception {
    /// Start of the occurrence as given by the pattern (UTC timestamp).
    pub original_start: i64,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_day: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy: Option<BusyStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<Option<i32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity: Option<Sensitivity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub uid: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub location: String,
    /// Plain-text description.
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_html: Option<String>,
    /// UTC timestamps. All-day events start and end at local midnight of `tz`.
    pub start: i64,
    pub end: i64,
    #[serde(default)]
    pub all_day: bool,
    /// IANA time zone the event was created in (series are expanded in it).
    #[serde(default)]
    pub tz: String,
    #[serde(default)]
    pub busy: BusyStatus,
    #[serde(default)]
    pub sensitivity: Sensitivity,
    /// Minutes before start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organizer: Option<Person>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<Attendee>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Recurrence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exceptions: Vec<Exception>,
    /// Own response to a meeting.
    #[serde(default)]
    pub response: Response,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub dtstamp: i64,
    /// iCalendar SEQUENCE.
    #[serde(default)]
    pub sequence: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub online_meeting_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub original_start: i64,
    pub start: i64,
    pub end: i64,
    pub exception: Option<Exception>,
}

/// Default time zone of the server (set from the configuration).
static DEFAULT_TZ: std::sync::OnceLock<Tz> = std::sync::OnceLock::new();

pub fn set_default_tz(tz: Tz) {
    let _ = DEFAULT_TZ.set(tz);
}

pub fn default_tz() -> Tz {
    *DEFAULT_TZ.get().unwrap_or(&chrono_tz::UTC)
}

pub fn tz_or_default(name: &str) -> Tz {
    name.parse().unwrap_or_else(|_| default_tz())
}

fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(y, m, 1).expect("valid date").pred_opt().expect("valid date").day()
}

fn add_months(year: i32, month: u32, add: i64) -> (i32, u32) {
    let total = year as i64 * 12 + (month as i64 - 1) + add;
    ((total.div_euclid(12)) as i32, (total.rem_euclid(12) + 1) as u32)
}

/// The `week`th day of `month` matching `days` (5 = last). Several days in
/// the mask (e.g. "weekday") count as one combined list of candidate dates.
fn nth_day(year: i32, month: u32, week: u32, days: u8) -> Option<NaiveDate> {
    let last = last_day_of_month(year, month);
    let matching: Vec<NaiveDate> = (1..=last)
        .filter_map(|d| NaiveDate::from_ymd_opt(year, month, d))
        .filter(|d| days & day_bit(d.weekday()) != 0)
        .collect();
    if week >= 5 { matching.last().copied() } else { matching.get(week.saturating_sub(1) as usize).copied() }
}

fn to_utc(tz: Tz, local: NaiveDateTime) -> i64 {
    tz.from_local_datetime(&local)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(local + Duration::hours(1))).earliest())
        .map(|d| d.timestamp())
        .unwrap_or_else(|| local.and_utc().timestamp())
}

impl Event {
    pub fn zone(&self) -> Tz {
        tz_or_default(&self.tz)
    }

    pub fn start_local(&self) -> NaiveDateTime {
        DateTime::from_timestamp(self.start, 0).unwrap_or_default().with_timezone(&self.zone()).naive_local()
    }


    /// Start dates (local) generated by the pattern, ascending, up to
    /// `until_local` (inclusive), honouring count and until.
    fn pattern_dates(&self, until_local: NaiveDate) -> Vec<NaiveDate> {
        let Some(r) = &self.recurrence else { return vec![self.start_local().date()] };
        let mut r = r.clone();
        let start = self.start_local();
        r.normalize(start);
        let first = start.date();
        let tz = self.zone();
        let until = r.until.map(|u| DateTime::from_timestamp(u, 0).unwrap_or_default().with_timezone(&tz).date_naive());
        let limit = until.map_or(until_local, |u| u.min(until_local));
        let count = r.count.map(|c| c as usize).unwrap_or(usize::MAX);
        let interval = r.interval.max(1) as i64;
        let mut out = Vec::new();
        let push = |d: NaiveDate, out: &mut Vec<NaiveDate>| -> bool {
            // false = stop
            if d < first {
                return true;
            }
            if d > limit || out.len() >= count {
                return false;
            }
            out.push(d);
            true
        };
        const MAX_STEPS: i64 = 200_000;
        match r.freq {
            Frequency::Daily if r.days != 0 => {
                // "Every weekday" style daily patterns.
                let mut d = first;
                for _ in 0..MAX_STEPS {
                    if r.days & day_bit(d.weekday()) != 0 && !push(d, &mut out) {
                        break;
                    }
                    if d > limit {
                        break;
                    }
                    d = d.succ_opt().unwrap_or(d);
                }
            }
            Frequency::Daily => {
                let mut d = first;
                for _ in 0..MAX_STEPS {
                    if !push(d, &mut out) {
                        break;
                    }
                    d += Duration::days(interval);
                }
            }
            Frequency::Weekly => {
                let fdow = WEEKDAYS[(r.first_day_of_week % 7) as usize];
                let offset = (7 + first.weekday().num_days_from_sunday() as i64 - fdow.num_days_from_sunday() as i64) % 7;
                let mut week_start = first - Duration::days(offset);
                'outer: for _ in 0..MAX_STEPS {
                    for i in 0..7 {
                        let d = week_start + Duration::days(i);
                        if r.days & day_bit(d.weekday()) != 0 && !push(d, &mut out) {
                            break 'outer;
                        }
                    }
                    week_start += Duration::days(7 * interval);
                    if week_start > limit {
                        break;
                    }
                }
            }
            Frequency::Monthly | Frequency::MonthlyNth | Frequency::Yearly | Frequency::YearlyNth => {
                let yearly = matches!(r.freq, Frequency::Yearly | Frequency::YearlyNth);
                let step = if yearly { 12 * interval } else { interval };
                let (y0, m0) = if yearly { (first.year(), r.month.clamp(1, 12)) } else { (first.year(), first.month()) };
                // Yearly series whose month lies before the start month begin next year.
                for k in 0..MAX_STEPS {
                    let (y, m) = add_months(y0, m0, k * step);
                    let d = match r.freq {
                        Frequency::Monthly | Frequency::Yearly => {
                            let day = r.day_of_month.clamp(1, 31).min(last_day_of_month(y, m));
                            NaiveDate::from_ymd_opt(y, m, day)
                        }
                        _ => nth_day(y, m, r.week_of_month, r.days),
                    };
                    let Some(d) = d else { continue };
                    if d > limit {
                        break;
                    }
                    if !push(d, &mut out) {
                        break;
                    }
                }
            }
        }
        out
    }

    /// Occurrences overlapping `[from, to)` (UTC timestamps), exceptions applied.
    pub fn occurrences(&self, from: i64, to: i64) -> Vec<Occurrence> {
        let duration = (self.end - self.start).max(0);
        if self.recurrence.is_none() {
            if self.start < to && (self.end > from || (self.end == self.start && self.start >= from)) {
                return vec![Occurrence { original_start: self.start, start: self.start, end: self.end, exception: None }];
            }
            return vec![];
        }
        // Year 9000 at most: far enough for "never ending" and representable.
        let to = to.min(220_898_000_000);
        let tz = self.zone();
        let time: NaiveTime = self.start_local().time();
        // Moved exceptions may land in the window although their pattern date does not.
        let max_shift = self
            .exceptions
            .iter()
            .filter_map(|e| e.start.map(|s| (s - e.original_start).abs()))
            .max()
            .unwrap_or(0);
        let until_local = DateTime::from_timestamp(to + max_shift + 86_400, 0).unwrap_or_default().with_timezone(&tz).date_naive();
        let mut out = Vec::new();
        for d in self.pattern_dates(until_local) {
            let original = to_utc(tz, d.and_time(time));
            let exception = self.exceptions.iter().find(|e| e.original_start == original).cloned();
            if exception.as_ref().is_some_and(|e| e.deleted) {
                continue;
            }
            let start = exception.as_ref().and_then(|e| e.start).unwrap_or(original);
            let end = exception.as_ref().and_then(|e| e.end).unwrap_or(start + duration);
            if start < to && end > from {
                out.push(Occurrence { original_start: original, start, end, exception });
            }
        }
        out
    }


    /// End of the last occurrence (`None` = series without end), for the
    /// range index of the store.
    pub fn last_end(&self) -> Option<i64> {
        let Some(r) = &self.recurrence else { return Some(self.end) };
        if r.until.is_none() && r.count.is_none() {
            return None;
        }
        let far = self.start + 400 * 366 * 86_400;
        self.occurrences(self.start, far).iter().map(|o| o.end).max().or(Some(self.end))
    }

    pub fn exception_mut(&mut self, original_start: i64) -> &mut Exception {
        if let Some(i) = self.exceptions.iter().position(|e| e.original_start == original_start) {
            return &mut self.exceptions[i];
        }
        self.exceptions.push(Exception { original_start, ..Default::default() });
        self.exceptions.last_mut().expect("just pushed")
    }
}

/// Local midnight of `date` in `tz` as UTC timestamp.
pub fn local_midnight(tz: Tz, date: NaiveDate) -> i64 {
    to_utc(tz, date.and_hms_opt(0, 0, 0).expect("midnight"))
}


// ---------------------------------------------------------------- contacts

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    #[serde(default)]
    pub street: String,
    #[serde(default)]
    pub city: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub postal_code: String,
    #[serde(default)]
    pub country: String,
}

impl Address {
    pub fn is_empty(&self) -> bool {
        self.street.is_empty() && self.city.is_empty() && self.state.is_empty() && self.postal_code.is_empty() && self.country.is_empty()
    }
}

/// Contact fields follow the Exchange contact schema (MS-ASCNTC / EWS Contact).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Contact {
    pub file_as: String,
    pub display_name: String,
    pub title: String,
    pub first_name: String,
    pub middle_name: String,
    pub last_name: String,
    pub suffix: String,
    pub nickname: String,
    pub company: String,
    pub department: String,
    pub job_title: String,
    pub office: String,
    pub manager: String,
    pub assistant: String,
    pub spouse: String,
    pub children: Vec<String>,
    /// Up to three addresses (Email1–3).
    pub emails: Vec<String>,
    pub im: Vec<String>,
    pub business_phone: String,
    pub business_phone2: String,
    pub business_fax: String,
    pub home_phone: String,
    pub home_phone2: String,
    pub home_fax: String,
    pub mobile_phone: String,
    pub car_phone: String,
    pub pager: String,
    pub radio_phone: String,
    pub assistant_phone: String,
    pub company_main_phone: String,
    pub business_address: Address,
    pub home_address: Address,
    pub other_address: Address,
    pub web_page: String,
    /// `YYYY-MM-DD`.
    pub birthday: Option<String>,
    pub anniversary: Option<String>,
    pub notes: String,
    pub categories: Vec<String>,
    /// Base64 JPEG.
    pub picture: Option<String>,
}

impl Contact {
    pub fn effective_name(&self) -> String {
        if !self.display_name.is_empty() {
            return self.display_name.clone();
        }
        let name = [self.first_name.as_str(), self.middle_name.as_str(), self.last_name.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if !name.is_empty() {
            return name;
        }
        if !self.company.is_empty() {
            return self.company.clone();
        }
        self.emails.first().cloned().unwrap_or_default()
    }
}

// ------------------------------------------------------------------- tasks

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
}

impl Importance {
    pub fn eas(self) -> u8 {
        self as u8
    }

    pub fn from_eas(v: &str) -> Self {
        match v.trim() {
            "0" => Importance::Low,
            "2" => Importance::High,
            _ => Importance::Normal,
        }
    }

    pub fn ews(self) -> &'static str {
        match self {
            Importance::Low => "Low",
            Importance::Normal => "Normal",
            Importance::High => "High",
        }
    }

    pub fn from_ews(v: &str) -> Self {
        match v {
            "Low" => Importance::Low,
            "High" => Importance::High,
            _ => Importance::Normal,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub subject: String,
    pub body: String,
    /// UTC timestamps.
    pub start: Option<i64>,
    pub due: Option<i64>,
    pub complete: bool,
    pub completed_at: Option<i64>,
    /// 0–100.
    pub percent_complete: u8,
    pub importance: Importance,
    pub sensitivity: Sensitivity,
    pub reminder: Option<i64>,
    pub categories: Vec<String>,
    pub recurrence: Option<Recurrence>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Note {
    pub subject: String,
    pub body: String,
    pub categories: Vec<String>,
}

// -------------------------------------------------------------------- mail

/// Message flags stored in the `flags` column.
pub mod flags {
    pub const READ: i64 = 1;
    pub const FLAGGED: i64 = 2;
    pub const ANSWERED: i64 = 4;
    pub const FORWARDED: i64 = 8;
    pub const DRAFT: i64 = 16;
    pub const COMPLETE: i64 = 32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(ts: i64) -> DateTime<chrono::Utc> {
        DateTime::from_timestamp(ts, 0).unwrap()
    }

    fn berlin(y: i32, m: u32, d: u32, h: u32) -> i64 {
        to_utc(chrono_tz::Europe::Berlin, NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap())
    }

    fn event(start: i64, rec: Recurrence) -> Event {
        Event { uid: "u".into(), start, end: start + 3600, tz: "Europe/Berlin".into(), recurrence: Some(rec), ..Default::default() }
    }

    fn rec(freq: Frequency) -> Recurrence {
        Recurrence { freq, interval: 1, days: 0, day_of_month: 0, week_of_month: 0, month: 0, until: None, count: None, first_day_of_week: 1 }
    }

    #[test]
    fn weekly_series_keeps_local_time_across_dst() {
        // Mondays 09:00 Berlin from 2026-10-19 (CEST) into November (CET).
        let mut r = rec(Frequency::Weekly);
        r.days = day_bit(Weekday::Mon) | day_bit(Weekday::Thu);
        r.count = Some(5);
        let e = event(berlin(2026, 10, 19, 9), r);
        let occ = e.occurrences(berlin(2026, 1, 1, 0), berlin(2027, 1, 1, 0));
        let starts: Vec<i64> = occ.iter().map(|o| o.start).collect();
        assert_eq!(starts, vec![berlin(2026, 10, 19, 9), berlin(2026, 10, 22, 9), berlin(2026, 10, 26, 9), berlin(2026, 10, 29, 9), berlin(2026, 11, 2, 9)]);
        assert_eq!(e.last_end(), Some(berlin(2026, 11, 2, 10)));
    }

    #[test]
    fn monthly_patterns() {
        let mut r = rec(Frequency::Monthly);
        r.day_of_month = 31;
        let e = event(berlin(2026, 1, 31, 8), r);
        let occ = e.occurrences(berlin(2026, 2, 1, 0), berlin(2026, 5, 1, 0));
        let days: Vec<u32> = occ.iter().map(|o| utc(o.start).with_timezone(&chrono_tz::Europe::Berlin).day()).collect();
        assert_eq!(days, vec![28, 31, 30]);

        // Last Friday of every month.
        let mut r = rec(Frequency::MonthlyNth);
        r.days = day_bit(Weekday::Fri);
        r.week_of_month = 5;
        let e = event(berlin(2026, 11, 27, 15), r);
        let occ = e.occurrences(berlin(2026, 11, 1, 0), berlin(2027, 2, 1, 0));
        assert_eq!(occ.len(), 3);
        assert_eq!(occ[1].start, berlin(2026, 12, 25, 15));
    }

    #[test]
    fn exceptions_move_and_delete() {
        let mut e = event(berlin(2026, 11, 2, 9), rec(Frequency::Daily));
        let second = berlin(2026, 11, 3, 9);
        let third = berlin(2026, 11, 4, 9);
        e.exception_mut(second).deleted = true;
        let ex = e.exception_mut(third);
        ex.start = Some(berlin(2026, 11, 4, 14));
        ex.end = Some(berlin(2026, 11, 4, 15));
        ex.subject = Some("Moved".into());
        let occ = e.occurrences(berlin(2026, 11, 2, 0), berlin(2026, 11, 5, 0));
        assert_eq!(occ.len(), 2);
        assert_eq!(occ[1].start, berlin(2026, 11, 4, 14));
        assert_eq!(occ[1].exception.as_ref().unwrap().subject.as_deref(), Some("Moved"));
        assert_eq!(e.last_end(), None);
    }

    #[test]
    fn yearly_birthday() {
        let mut r = rec(Frequency::Yearly);
        r.until = Some(berlin(2030, 1, 1, 0));
        let mut e = event(local_midnight(chrono_tz::Europe::Berlin, NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()), r);
        e.all_day = true;
        e.end = e.start + 86_400;
        let occ = e.occurrences(0, i64::MAX / 2);
        assert_eq!(occ.len(), 6);
        assert_eq!(utc(occ[1].start).with_timezone(&chrono_tz::Europe::Berlin).day(), 28);
    }
}
