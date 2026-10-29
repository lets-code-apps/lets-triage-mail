//! The daily digest: one summary email per account, written into the mailbox.
//!
//! The pieces, from the inside out:
//!
//! - [`build`]: stored rows in, a [`build::Digest`] model out. Pure.
//! - [`render`]: the model to a plain-text part and an HTML part, and the finished
//!   message with the header image inside it. Pure.
//! - [`schedule`]: when a digest is due ([`schedule::next_step`], pure) and the loop that
//!   sends it every day, with catch-up after the Mac sleeps.
//! - [`deliver`]: reads the store, `APPEND`s the message (or saves a preview file for a
//!   dry-run account) and records that it did.
//!
//! # Decisions worth knowing
//!
//! - **English only.** `[digest] language` must be `en`; anything else is a config error.
//! - **No model calls.** Message bodies are never stored, so a summary written by the model
//!   would mean downloading the mail again. Each line uses the verdict's one-sentence
//!   `reason`, which the model already wrote while classifying.
//! - **A dry-run account is never appended to.** Its digest is a file under the data
//!   directory instead, so the owner can preview it.
//! - **The worker skips our digests by record, not by header.** See [`deliver`].
//! - **A quiet day still gets a digest.** "Nothing needs you" tells the owner
//!   the service is alive; a missing digest would be ambiguous.

pub mod build;
pub mod deliver;
pub mod render;
pub mod schedule;
