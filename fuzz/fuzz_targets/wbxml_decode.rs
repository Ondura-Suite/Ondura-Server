#![no_main]
//! Fuzz the WBXML decoder with arbitrary bytes. A hostile ActiveSync client can
//! send any byte sequence here, so decoding must never panic or run away — it
//! must only ever return `Ok` or a decode `Err`.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = ondura_wbxml::Element::decode(data);
});
