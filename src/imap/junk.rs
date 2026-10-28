//! The Spam-folder watch: trusted mail that the provider filed as spam.
//!
//! Found on 2026-10-25: wp.pl put a DKIM-valid message from the user's own Gmail address into
//! `Spam`, and the worker never saw it, because it only watches the inbox. [`scan_junk`] looks
//! into the Spam folder and records a message as **possibly misfiled** when its sender is
//! trusted (an allow rule, or someone the user has written to) and its DKIM signature passed
//! ([`possibly_misfiled`]). The daily digest lists those. Everything else in the folder is
//! ignored.
//!
//! # Read-only, and no model
//!
//! The folder is opened with `EXAMINE`, so the server refuses any change to it, and only the
//! headers are fetched, with `PEEK`, so nothing is marked as read. Nothing is moved. The model
//! is never asked: a Spam folder holds hundreds of messages, and the only question is "do I
//! know this sender?". A source-scan test below fails if a writing call ever appears here.
//!
//! # When it runs
//!
//! On the inbox worker's own connection, at a wake-up, at most once per
//! `WorkerTiming::junk_interval` (15 minutes), instead of on a second connection per account.
//! Reasons: some providers limit parallel connections; the worker re-selects the inbox at every
//! wake-up anyway, so looking into another folder first costs one `EXAMINE`; and nothing here is
//! urgent, since the digest is written once a day.
//!
//! # Position
//!
//! Like the inbox, the folder's `(UIDVALIDITY, last_uid)` is kept in `mailbox_state`, and the
//! first run starts from now ([`plan_cursor`]): mail already in Spam is history.

use std::sync::{Arc, Mutex};

use super::ImapSession;
use super::worker::{CursorChange, WorkerError, plan_cursor};
use crate::config::AccountConfig;
use crate::engine::{Trust, screen_sender};
use crate::message::{AuthVerdict, parse_headers_only};
use crate::senders::{plan_batches, with_store};
use crate::store::{MailboxState, MessageKey, MisfiledRecord, Store};

/// How many UIDs one metadata fetch covers.
const JUNK_BATCH: u32 = 100;

/// What one look into the Spam folder did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JunkReport {
    /// The folder that was read; `None` when the watch is off or no Spam folder was found.
    pub folder: Option<String>,
    /// New messages whose headers were read.
    pub scanned: u32,
    /// Of those, how many were recorded as possibly misfiled.
    pub misfiled: u32,
}

/// A message in Spam is possibly misfiled when its sender is trusted and its DKIM signature
/// passed. A trusted address alone is not enough: the `From` header is easy to forge, and
/// forged mail is exactly what a Spam folder holds.
pub fn possibly_misfiled(trust: Trust, dkim: AuthVerdict) -> bool {
    trust == Trust::Trusted && dkim == AuthVerdict::Pass
}

/// Read the new messages in the account's Spam folder and record the possibly misfiled ones.
///
/// Uses the configured `junk_folder`, or the folder the server marks `\Junk` (or names like
/// one). Does nothing when `watch_junk` is off or no folder is found. Leaves the session with
/// the Spam folder examined, so the caller must select the inbox again.
pub async fn scan_junk(
    session: &mut ImapSession,
    store: &Arc<Mutex<Store>>,
    account: &AccountConfig,
    authserv_id: Option<&str>,
) -> Result<JunkReport, WorkerError> {
    if !account.watch_junk {
        return Ok(JunkReport::default());
    }
    let folder = if account.junk_folder.trim().is_empty() {
        session.resolve_folders().await?.junk
    } else {
        Some(account.junk_folder.clone())
    };
    let Some(folder) = folder else {
        tracing::debug!(account = %account.name, "no Spam folder found; nothing to watch");
        return Ok(JunkReport::default());
    };

    let info = session.examine(&folder).await?;
    let saved = with_store(store, {
        let (name, folder) = (account.name.clone(), folder.clone());
        move |s| s.mailbox_state(&name, &folder)
    })
    .await?;
    let (mut state, change) = plan_cursor(saved, info.uid_validity, info.uid_next);
    if change != CursorChange::Unchanged {
        save_state(store, account, &folder, state).await?;
    }

    let mut report = JunkReport {
        folder: Some(folder.clone()),
        ..JunkReport::default()
    };
    for (first, last) in plan_batches(state.last_uid, info.uid_next, JUNK_BATCH) {
        for meta in session.fetch_meta_range(first, last).await? {
            let Some(raw) = session.fetch_headers_peek(meta.uid).await? else {
                continue;
            };
            report.scanned += 1;
            let Ok(parsed) = parse_headers_only(&raw, authserv_id, account.provider_headers) else {
                continue;
            };
            if parsed.from_address.is_empty() {
                continue;
            }

            let (name, sender) = (account.name.clone(), parsed.from_address.clone());
            let (rule, known) = with_store(store, move |s| {
                Ok((
                    s.match_rule(&name, &sender)?,
                    s.is_known_sender(&name, &sender)?,
                ))
            })
            .await?;
            let trust = screen_sender(rule, known, parsed.auth.any_fail());
            if !possibly_misfiled(trust, parsed.auth.dkim) {
                continue;
            }

            let record = MisfiledRecord {
                key: MessageKey {
                    account: account.name.clone(),
                    mailbox: folder.clone(),
                    uidvalidity: state.uidvalidity,
                    uid: meta.uid,
                },
                sender: parsed.from_address.clone(),
                subject: parsed.subject.clone(),
                date: parsed.date.clone(),
            };
            with_store(store, move |s| s.record_misfiled(&record)).await?;
            report.misfiled += 1;
            tracing::info!(
                account = %account.name, uid = meta.uid,
                "possibly misfiled in Spam: a trusted sender with a valid DKIM signature"
            );
        }
        // UIDs in the range that no longer exist are simply passed over.
        state.last_uid = last;
        save_state(store, account, &folder, state).await?;
    }
    Ok(report)
}

/// Saves the position in the Spam folder.
async fn save_state(
    store: &Arc<Mutex<Store>>,
    account: &AccountConfig,
    folder: &str,
    state: MailboxState,
) -> Result<(), WorkerError> {
    let (name, folder) = (account.name.clone(), folder.to_owned());
    with_store(store, move |s| {
        s.set_mailbox_state(&name, &folder, state.uidvalidity, state.last_uid)
            .map(drop)
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trusted_senders_with_a_passing_dkim_are_misfiled() {
        assert!(possibly_misfiled(Trust::Trusted, AuthVerdict::Pass));
        for dkim in [
            AuthVerdict::Fail,
            AuthVerdict::SoftFail,
            AuthVerdict::Neutral,
            AuthVerdict::None,
            AuthVerdict::Error,
            AuthVerdict::Other,
        ] {
            assert!(!possibly_misfiled(Trust::Trusted, dkim), "{dkim:?}");
        }
        assert!(!possibly_misfiled(Trust::Unknown, AuthVerdict::Pass));
        assert!(!possibly_misfiled(Trust::Blocked, AuthVerdict::Pass));
    }

    #[test]
    fn junk_source_never_writes_to_the_server() {
        let production_code = include_str!("junk.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        // Built with `concat` so this test does not trip over its own text.
        for forbidden in [
            ["store", "_flags"].concat(),
            ["uid", "_store"].concat(),
            [".sel", "ect("].concat(),
            [".app", "end("].concat(),
            [".exp", "unge("].concat(),
            [".uid", "_expunge("].concat(),
            [".uid", "_mv("].concat(),
            [".mv", "("].concat(),
            [".uid", "_copy("].concat(),
            [".copy", "("].concat(),
            [".del", "ete("].concat(),
            ["\\Del", "eted"].concat(),
            ["fetch_body", "_peek"].concat(),
            ["decide", "("].concat(),
        ] {
            assert!(
                !production_code.contains(&forbidden),
                "the Spam watch must not use {forbidden}"
            );
        }
    }
}
