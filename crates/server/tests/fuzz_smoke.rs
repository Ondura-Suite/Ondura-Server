//! Randomised smoke tests for the untrusted-input parsers.
//!
//! These are not a substitute for coverage-guided fuzzing (see `fuzz/` and
//! `docs/fuzzing.md`), but they run on stable Rust in CI and catch the obvious
//! panics — out-of-bounds slicing, `unwrap` on malformed input, runaway
//! recursion — by throwing many random and mutated inputs at each parser and
//! asserting none of them panics.

use std::panic::{catch_unwind, AssertUnwindSafe};

/// Tiny deterministic PRNG (xorshift64*). Deterministic so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn byte(&mut self) -> u8 {
        (self.next() & 0xff) as u8
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Produce one input: either random bytes, or a mutated copy of a seed.
fn generate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    let mode = rng.below(3);
    if mode == 0 || seeds.is_empty() {
        let len = rng.below(512);
        return (0..len).map(|_| rng.byte()).collect();
    }
    let mut buf = seeds[rng.below(seeds.len())].clone();
    // Apply a handful of mutations: bit flips, overwrites, insertions, deletions.
    let rounds = 1 + rng.below(16);
    for _ in 0..rounds {
        if buf.is_empty() {
            buf.push(rng.byte());
            continue;
        }
        match rng.below(4) {
            0 => {
                let i = rng.below(buf.len());
                buf[i] ^= 1 << rng.below(8);
            }
            1 => {
                let i = rng.below(buf.len());
                buf[i] = rng.byte();
            }
            2 => {
                let i = rng.below(buf.len() + 1);
                buf.insert(i, rng.byte());
            }
            _ => {
                let i = rng.below(buf.len());
                buf.remove(i);
            }
        }
    }
    buf
}

const ITERATIONS: usize = 100_000;

#[test]
fn xml_parser_never_panics() {
    let seeds: Vec<Vec<u8>> = [
        r#"<?xml version="1.0"?><soap:Envelope xmlns:soap="x"><soap:Body><m:GetItem xmlns:m="y"><m:ItemShape><t:BaseShape>Default</t:BaseShape></m:ItemShape></m:GetItem></soap:Body></soap:Envelope>"#,
        r#"<a b="c&amp;d">text &lt;here&gt; <child/></a>"#,
        r#"<x><![CDATA[ raw < & > ]]></x>"#,
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();

    let mut rng = Rng(0x1234_5678_9abc_def0);
    for _ in 0..ITERATIONS {
        let input = generate(&mut rng, &seeds);
        let result = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(text) = std::str::from_utf8(&input) {
                let _ = ondura_server::xml::Node::parse(text);
            }
        }));
        assert!(result.is_ok(), "xml parser panicked on input: {input:?}");
    }
}

#[test]
fn mime_parser_never_panics() {
    let seeds: Vec<Vec<u8>> = [
        "From: a@b.c\r\nTo: d@e.f\r\nSubject: Hi\r\nContent-Type: text/plain\r\n\r\nBody\r\n",
        "Content-Type: multipart/mixed; boundary=xx\r\n\r\n--xx\r\nContent-Type: text/plain\r\n\r\nhi\r\n--xx--\r\n",
        "Subject: =?utf-8?B?SGVsbG8=?=\r\nDate: Mon, 1 Jan 2024 00:00:00 +0000\r\n\r\n",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();

    let mut rng = Rng(0x0fed_cba9_8765_4321);
    for _ in 0..ITERATIONS {
        let input = generate(&mut rng, &seeds);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = ondura_server::mime::parse_meta(&input);
        }));
        assert!(result.is_ok(), "mime parser panicked on input: {input:?}");
    }
}

#[test]
fn ical_parser_never_panics() {
    let seeds: Vec<Vec<u8>> = [
        "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:1\r\nDTSTART:20240101T100000Z\r\nDTEND:20240101T110000Z\r\nSUMMARY:Meet\r\nRRULE:FREQ=WEEKLY;COUNT=5\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n",
        "BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nEND:VTIMEZONE\r\nEND:VCALENDAR\r\n",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect();

    let mut rng = Rng(0xdead_beef_cafe_babe);
    for _ in 0..ITERATIONS {
        let input = generate(&mut rng, &seeds);
        let result = catch_unwind(AssertUnwindSafe(|| {
            if let Ok(text) = std::str::from_utf8(&input) {
                let _ = ondura_server::ical::parse(text);
            }
        }));
        assert!(result.is_ok(), "ical parser panicked on input: {input:?}");
    }
}
