//! Real-time collaborative documents (the "collab server" side of Ondura).
//!
//! This whole module is optional and gated behind `[collab] enabled` in the
//! configuration, so Ondura can run as a mail-only server.

pub mod docstore;
pub mod markdown;
pub mod ot;
pub mod server;

#[cfg(test)]
mod ot_tests;
#[cfg(test)]
mod docstore_tests;
