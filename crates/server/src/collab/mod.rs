//! Real-time collaborative documents (the "collab server" side of Ondura).
//!
//! This whole module is optional and gated behind `[collab] enabled` in the
//! configuration, so Ondura can run as a mail-only server.

pub mod ot;

#[cfg(test)]
mod ot_tests;
