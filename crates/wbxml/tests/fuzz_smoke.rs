//! Randomised smoke test for the WBXML decoder.
//!
//! Not a substitute for coverage-guided fuzzing (see `../../fuzz/`), but it runs
//! on stable Rust in CI and asserts the decoder never panics or overflows the
//! stack on arbitrary or mutated input — it must only ever return `Ok` or a
//! decode `Err`.

use ondura_wbxml::Element;
use std::panic::catch_unwind;

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

fn seeds() -> Vec<Vec<u8>> {
    vec![
        Element::new("FolderHierarchy:FolderSync").with_text("FolderHierarchy:SyncKey", "0").encode(),
        Element::new("AirSync:Sync")
            .with(Element::new("AirSync:Collections").with(Element::new("AirSync:Collection").with_text("AirSync:SyncKey", "1").with_text("AirSync:Status", "1")))
            .encode(),
    ]
}

fn generate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    if rng.below(3) == 0 {
        let len = rng.below(512);
        return (0..len).map(|_| rng.byte()).collect();
    }
    let mut buf = seeds[rng.below(seeds.len())].clone();
    for _ in 0..1 + rng.below(16) {
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

#[test]
fn wbxml_decoder_never_panics() {
    let seeds = seeds();
    let mut rng = Rng(0xa5a5_5a5a_f00d_1357);
    for _ in 0..100_000 {
        let input = generate(&mut rng, &seeds);
        let result = catch_unwind(|| {
            let _ = Element::decode(&input);
        });
        assert!(result.is_ok(), "wbxml decoder panicked on input: {input:?}");
    }
}
