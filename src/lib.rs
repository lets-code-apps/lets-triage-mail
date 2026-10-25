//! Let's Triage Mail: local-first email triage for any IMAP account.
//!
//! The library holds the logic; `main.rs` is a thin command-line front end over it.
//! See `docs/ARCHITECTURE.md` for the planned modules.

pub mod check;
pub mod config;
pub mod decider;
pub mod engine;
pub mod evidence;
pub mod flags;
pub mod imap;
pub mod message;
pub mod senders;
pub mod store;
pub mod supervisor;
