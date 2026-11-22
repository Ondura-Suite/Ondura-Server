#![no_main]
//! Fuzz the iCalendar parser. Meeting invitations carry attacker-influenced
//! iCalendar bodies; parsing must never panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = ondura_server::ical::parse(text);
    }
});
