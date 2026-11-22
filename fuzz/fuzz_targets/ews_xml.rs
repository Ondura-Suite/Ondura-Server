#![no_main]
//! Fuzz the SOAP/POX XML tree parser. EWS and Autodiscover requests arrive as
//! XML from authenticated clients; malformed input must return an `Err`, never
//! panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = ondura_server::xml::Node::parse(text);
    }
});
