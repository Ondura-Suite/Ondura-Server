#![no_main]
//! Fuzz MIME metadata extraction. Inbound SMTP hands arbitrary bytes to the
//! parser; it must extract what it can and never panic on malformed messages.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ondura_server::mime::parse_meta(data);
});
