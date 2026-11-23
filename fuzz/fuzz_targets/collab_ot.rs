#![no_main]
//! Fuzz the collaborative-editing OT wire path. A client sends arbitrary JSON
//! operations over the WebSocket; parsing, applying and transforming them must
//! never panic — only ever return a value or an error.
use libfuzzer_sys::fuzz_target;
use ondura_server::collab::ot::{to_u16, TextOp};

fuzz_target!(|data: &[u8]| {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) else { return };
    // Try the operation against a few base-document lengths.
    for base in [0u32, 5, 64] {
        let Ok(op) = TextOp::from_json(&value, base) else { continue };
        let doc = to_u16(&"a".repeat(base as usize));
        // Applying must not panic; on success the output is a valid document.
        if let Ok(after) = op.apply(&doc) {
            // Transform against a no-op over the same base and check both parsed
            // ops still compose/apply without panicking.
            let mut noop = TextOp::new();
            noop.retain(base);
            if let Ok((a_prime, b_prime)) = TextOp::transform(&op, &noop) {
                let _ = b_prime.apply(&after);
                let _ = noop.apply(&doc).map(|d| a_prime.apply(&d));
            }
            let _ = op.compose(&{
                let mut r = TextOp::new();
                r.retain(op.target_len());
                r
            });
        }
    }
});
