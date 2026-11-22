//! Windows-style time zone rules (bias + yearly transitions) derived from
//! IANA zones, as needed by ActiveSync (`TIME_ZONE_INFORMATION` blobs), EWS
//! (`TimeZoneDefinition`) and iCalendar (`VTIMEZONE`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Datelike, NaiveDate, Offset, TimeZone, Timelike, Utc};
use chrono_tz::{OffsetName, Tz};

/// A yearly transition: the `week`th (5 = last) `weekday` (0 = Sunday) of
/// `month`, at local wall-clock time `hour:minute` before the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Transition {
    pub month: u32,
    pub week: u32,
    pub weekday: u32,
    pub hour: u32,
    pub minute: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TzRule {
    /// Standard offset from UTC in minutes (east positive, e.g. +60 for CET).
    pub std_offset: i32,
    /// Daylight offset in minutes (equal to `std_offset` without DST).
    pub dst_offset: i32,
    /// Change to standard time (`None` without DST).
    pub to_std: Option<Transition>,
    /// Change to daylight time.
    pub to_dst: Option<Transition>,
    pub std_name: String,
    pub dst_name: String,
}

fn offset_at(tz: Tz, ts: i64) -> i32 {
    let dt = DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&tz);
    dt.offset().fix().local_minus_utc() / 60
}

/// Instants (UTC) in `year` at which the offset of `tz` changes.
fn changes(tz: Tz, year: i32) -> Vec<i64> {
    let start = Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap().timestamp();
    let end = Utc.with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0).unwrap().timestamp();
    let mut out = Vec::new();
    let step = 86_400;
    let mut t = start;
    let mut prev = offset_at(tz, t);
    while t < end {
        let next = (t + step).min(end);
        let o = offset_at(tz, next);
        if o != prev {
            // Bisect to the minute.
            let (mut lo, mut hi) = (t, next);
            while hi - lo > 60 {
                let mid = lo + (hi - lo) / 2;
                if offset_at(tz, mid) == prev {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            out.push(hi);
            prev = o;
        }
        t = next;
    }
    out
}

fn transition_at(ts: i64, offset_before: i32) -> Transition {
    let local = DateTime::from_timestamp(ts + offset_before as i64 * 60, 0).unwrap_or_default().naive_utc();
    let d = local.date();
    let last = days_in_month(d.year(), d.month());
    let week = if d.day() + 7 > last { 5 } else { (d.day() - 1) / 7 + 1 };
    Transition { month: d.month(), week, weekday: d.weekday().num_days_from_sunday(), hour: local.hour(), minute: local.minute() }
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    NaiveDate::from_ymd_opt(ny, nm, 1).unwrap().pred_opt().unwrap().day()
}

/// Rule of `tz` in `year`.
pub fn rule(tz: Tz, year: i32) -> TzRule {
    let mid_jan = Utc.with_ymd_and_hms(year, 1, 15, 12, 0, 0).unwrap().timestamp();
    let mid_jul = Utc.with_ymd_and_hms(year, 7, 15, 12, 0, 0).unwrap().timestamp();
    let (o1, o2) = (offset_at(tz, mid_jan), offset_at(tz, mid_jul));
    let std_offset = o1.min(o2);
    let dst_offset = o1.max(o2);
    let name = |ts: i64| DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&tz).offset().abbreviation().unwrap_or("").to_string();
    let (std_ts, dst_ts) = if o1 <= o2 { (mid_jan, mid_jul) } else { (mid_jul, mid_jan) };
    let mut r = TzRule { std_offset, dst_offset, to_std: None, to_dst: None, std_name: name(std_ts), dst_name: name(dst_ts) };
    if std_offset == dst_offset {
        r.dst_name = r.std_name.clone();
        return r;
    }
    let mut offset = offset_at(tz, Utc.with_ymd_and_hms(year, 1, 1, 0, 0, 0).unwrap().timestamp());
    for ts in changes(tz, year) {
        let after = offset_at(tz, ts);
        let t = transition_at(ts, offset);
        if after == dst_offset {
            r.to_dst = Some(t);
        } else {
            r.to_std = Some(t);
        }
        offset = after;
    }
    if r.to_dst.is_none() || r.to_std.is_none() {
        // Permanent change during the year: treat as fixed offset.
        r.to_dst = None;
        r.to_std = None;
        r.std_offset = offset;
        r.dst_offset = offset;
    }
    r
}

pub fn current_year() -> i32 {
    Utc::now().year()
}

// ---------------------------------------------------- ActiveSync TimeZone

fn utf16_name(out: &mut Vec<u8>, s: &str) {
    let mut units: Vec<u16> = s.encode_utf16().take(31).collect();
    units.resize(32, 0);
    for u in units {
        out.extend_from_slice(&u.to_le_bytes());
    }
}

fn systemtime(out: &mut Vec<u8>, t: Option<Transition>) {
    let t = t.unwrap_or_default();
    for v in [0u16, t.month as u16, t.weekday as u16, t.week as u16, t.hour as u16, t.minute as u16, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Base64 `TIME_ZONE_INFORMATION` (MS-ASDTYPE 2.6.4) for `tz`.
pub fn eas_blob(tz: Tz) -> String {
    let r = rule(tz, current_year());
    let mut b = Vec::with_capacity(172);
    b.extend_from_slice(&(-r.std_offset).to_le_bytes());
    utf16_name(&mut b, &r.std_name);
    systemtime(&mut b, r.to_std);
    b.extend_from_slice(&0i32.to_le_bytes());
    utf16_name(&mut b, &r.dst_name);
    systemtime(&mut b, r.to_dst);
    b.extend_from_slice(&(-(r.dst_offset - r.std_offset)).to_le_bytes());
    B64.encode(b)
}

/// Parse an ActiveSync time zone blob.
pub fn parse_eas_blob(b64: &str) -> Option<TzRule> {
    let b = B64.decode(b64.trim()).ok()?;
    if b.len() < 172 {
        return None;
    }
    let i32_at = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u32;
    let name = |o: usize| {
        let units: Vec<u16> = (0..32).map(|i| u16_at(o + i * 2) as u16).take_while(|u| *u != 0).collect();
        String::from_utf16_lossy(&units)
    };
    let st = |o: usize| {
        let month = u16_at(o + 2);
        (month != 0).then(|| Transition { month, weekday: u16_at(o + 4), week: u16_at(o + 6), hour: u16_at(o + 8), minute: u16_at(o + 10) })
    };
    let bias = i32_at(0);
    let std_bias = i32_at(84);
    let dst_bias = i32_at(168);
    let to_std = st(68);
    let to_dst = st(152);
    let std_offset = -(bias + std_bias);
    let dst_offset = if to_dst.is_some() { -(bias + dst_bias) } else { std_offset };
    Some(TzRule { std_offset, dst_offset, to_std: to_dst.and(to_std), to_dst: to_std.and(to_dst), std_name: name(4), dst_name: name(88) })
}

/// Whether two rules describe the same clock (names are ignored).
pub fn same_clock(a: &TzRule, b: &TzRule) -> bool {
    a.std_offset == b.std_offset && a.dst_offset == b.dst_offset && a.to_std == b.to_std && a.to_dst == b.to_dst
}

/// An IANA zone following `rule` (the server default zone when it fits).
pub fn zone_for_rule(rule_in: &TzRule) -> Tz {
    static CACHE: OnceLock<Mutex<HashMap<String, Tz>>> = OnceLock::new();
    let key = format!("{:?}", (rule_in.std_offset, rule_in.dst_offset, rule_in.to_std, rule_in.to_dst));
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(tz) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return *tz;
    }
    let year = current_year();
    let default = crate::model::default_tz();
    let found = if same_clock(&rule(default, year), rule_in) {
        Some(default)
    } else {
        // Common zones first, then everything.
        const COMMON: &[&str] = &[
            "Europe/Berlin", "Europe/London", "Europe/Helsinki", "Europe/Moscow", "Europe/Lisbon", "America/New_York", "America/Chicago", "America/Denver",
            "America/Los_Angeles", "America/Phoenix", "America/Sao_Paulo", "Asia/Tokyo", "Asia/Shanghai", "Asia/Kolkata", "Asia/Dubai", "Australia/Sydney",
            "Pacific/Auckland", "UTC",
        ];
        COMMON
            .iter()
            .filter_map(|n| n.parse::<Tz>().ok())
            .chain(chrono_tz::TZ_VARIANTS.iter().copied())
            .find(|tz| same_clock(&rule(*tz, year), rule_in))
    };
    let tz = found.unwrap_or_else(|| fixed_zone(rule_in.std_offset));
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, tz);
    tz
}

/// Etc/GMT zone for a fixed offset (whole hours; others fall back to UTC).
pub fn fixed_zone(offset_minutes: i32) -> Tz {
    if offset_minutes % 60 != 0 {
        return chrono_tz::UTC;
    }
    let h = -offset_minutes / 60;
    let name = match h.cmp(&0) {
        std::cmp::Ordering::Equal => "Etc/GMT".to_string(),
        std::cmp::Ordering::Greater => format!("Etc/GMT+{h}"),
        std::cmp::Ordering::Less => format!("Etc/GMT{h}"),
    };
    name.parse().unwrap_or(chrono_tz::UTC)
}

// ------------------------------------------------------------- iCalendar

const DAYS: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];

fn ical_offset(m: i32) -> String {
    let sign = if m < 0 { '-' } else { '+' };
    format!("{sign}{:02}{:02}", m.abs() / 60, m.abs() % 60)
}

/// `VTIMEZONE` block for `tz` (current rules, valid from 1970).
pub fn vtimezone(tz: Tz) -> String {
    let r = rule(tz, current_year());
    let mut s = format!("BEGIN:VTIMEZONE\r\nTZID:{}\r\n", tz.name());
    match (r.to_std, r.to_dst) {
        (Some(std), Some(dst)) => {
            let comp = |kind: &str, t: Transition, from: i32, to: i32, name: &str| {
                let byday = if t.week == 5 { format!("-1{}", DAYS[t.weekday as usize]) } else { format!("{}{}", t.week, DAYS[t.weekday as usize]) };
                format!(
                    "BEGIN:{kind}\r\nDTSTART:19700101T{:02}{:02}00\r\nTZOFFSETFROM:{}\r\nTZOFFSETTO:{}\r\nTZNAME:{name}\r\nRRULE:FREQ=YEARLY;BYMONTH={};BYDAY={byday}\r\nEND:{kind}\r\n",
                    t.hour,
                    t.minute,
                    ical_offset(from),
                    ical_offset(to),
                    t.month
                )
            };
            s.push_str(&comp("STANDARD", std, r.dst_offset, r.std_offset, &r.std_name));
            s.push_str(&comp("DAYLIGHT", dst, r.std_offset, r.dst_offset, &r.dst_name));
        }
        _ => {
            s.push_str(&format!(
                "BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:{0}\r\nTZOFFSETTO:{0}\r\nTZNAME:{1}\r\nEND:STANDARD\r\n",
                ical_offset(r.std_offset),
                r.std_name
            ));
        }
    }
    s.push_str("END:VTIMEZONE\r\n");
    s
}

/// Windows time zone id for common IANA zones (used by EWS clients that
/// show the zone by name). Unknown zones get their IANA name.
pub fn windows_name(tz: Tz) -> String {
    let n = match tz.name() {
        "Europe/Berlin" | "Europe/Amsterdam" | "Europe/Vienna" | "Europe/Zurich" | "Europe/Rome" | "Europe/Stockholm" | "Europe/Oslo" => {
            "W. Europe Standard Time"
        }
        "Europe/Paris" | "Europe/Brussels" | "Europe/Madrid" | "Europe/Copenhagen" => "Romance Standard Time",
        "Europe/Warsaw" | "Europe/Zagreb" | "Europe/Sarajevo" | "Europe/Skopje" => "Central European Standard Time",
        "Europe/Prague" | "Europe/Budapest" | "Europe/Belgrade" | "Europe/Bratislava" | "Europe/Ljubljana" => "Central Europe Standard Time",
        "Europe/London" | "Europe/Dublin" | "Europe/Lisbon" => "GMT Standard Time",
        "Europe/Helsinki" | "Europe/Kiev" | "Europe/Kyiv" | "Europe/Riga" | "Europe/Sofia" | "Europe/Tallinn" | "Europe/Vilnius" => "FLE Standard Time",
        "Europe/Athens" | "Europe/Bucharest" => "GTB Standard Time",
        "Europe/Moscow" => "Russian Standard Time",
        "Europe/Istanbul" => "Turkey Standard Time",
        "America/New_York" => "Eastern Standard Time",
        "America/Chicago" => "Central Standard Time",
        "America/Denver" => "Mountain Standard Time",
        "America/Phoenix" => "US Mountain Standard Time",
        "America/Los_Angeles" => "Pacific Standard Time",
        "America/Sao_Paulo" => "E. South America Standard Time",
        "Asia/Tokyo" => "Tokyo Standard Time",
        "Asia/Shanghai" => "China Standard Time",
        "Asia/Kolkata" => "India Standard Time",
        "Asia/Dubai" => "Arabian Standard Time",
        "Australia/Sydney" => "AUS Eastern Standard Time",
        "Pacific/Auckland" => "New Zealand Standard Time",
        "UTC" | "Etc/UTC" | "Etc/GMT" => "UTC",
        other => return other.to_string(),
    };
    n.to_string()
}

/// IANA zone for a Windows time zone id or IANA name.
pub fn from_windows_name(name: &str) -> Option<Tz> {
    if let Ok(tz) = name.parse::<Tz>() {
        return Some(tz);
    }
    let iana = match name {
        "W. Europe Standard Time" => "Europe/Berlin",
        "Romance Standard Time" => "Europe/Paris",
        "Central European Standard Time" => "Europe/Warsaw",
        "Central Europe Standard Time" => "Europe/Budapest",
        "GMT Standard Time" => "Europe/London",
        "Greenwich Standard Time" => "Atlantic/Reykjavik",
        "FLE Standard Time" => "Europe/Helsinki",
        "GTB Standard Time" => "Europe/Bucharest",
        "Russian Standard Time" => "Europe/Moscow",
        "Turkey Standard Time" => "Europe/Istanbul",
        "Eastern Standard Time" => "America/New_York",
        "Central Standard Time" => "America/Chicago",
        "Mountain Standard Time" => "America/Denver",
        "US Mountain Standard Time" => "America/Phoenix",
        "Pacific Standard Time" => "America/Los_Angeles",
        "E. South America Standard Time" => "America/Sao_Paulo",
        "Tokyo Standard Time" => "Asia/Tokyo",
        "China Standard Time" => "Asia/Shanghai",
        "India Standard Time" => "Asia/Kolkata",
        "Arabian Standard Time" => "Asia/Dubai",
        "AUS Eastern Standard Time" => "Australia/Sydney",
        "New Zealand Standard Time" => "Pacific/Auckland",
        "UTC" | "Coordinated Universal Time" => "UTC",
        _ => return None,
    };
    iana.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn berlin_rule() {
        let r = rule(chrono_tz::Europe::Berlin, 2026);
        assert_eq!((r.std_offset, r.dst_offset), (60, 120));
        assert_eq!(r.to_dst, Some(Transition { month: 3, week: 5, weekday: 0, hour: 2, minute: 0 }));
        assert_eq!(r.to_std, Some(Transition { month: 10, week: 5, weekday: 0, hour: 3, minute: 0 }));
        assert_eq!(r.std_name, "CET");
    }

    #[test]
    fn eas_blob_round_trip() {
        let blob = eas_blob(chrono_tz::Europe::Berlin);
        assert_eq!(B64.decode(&blob).unwrap().len(), 172);
        let r = parse_eas_blob(&blob).unwrap();
        assert!(same_clock(&r, &rule(chrono_tz::Europe::Berlin, current_year())));
        assert_eq!(zone_for_rule(&r).name(), "Europe/Berlin");
        let ny = parse_eas_blob(&eas_blob(chrono_tz::America::New_York)).unwrap();
        assert_eq!(zone_for_rule(&ny).name(), "America/New_York");
        let tokyo = parse_eas_blob(&eas_blob(chrono_tz::Asia::Tokyo)).unwrap();
        assert_eq!(tokyo.std_offset, 540);
        assert!(tokyo.to_dst.is_none());
    }

    #[test]
    fn vtimezone_block() {
        let v = vtimezone(chrono_tz::Europe::Berlin);
        assert!(v.contains("RRULE:FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU"));
        assert!(v.contains("TZOFFSETTO:+0200"));
    }
}
