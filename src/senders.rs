//! Known senders: everyone the user has written to, harvested from the Sent folder.
//!
//! The screening step trusts a sender the user has written to (unless authentication fails),
//! so before the first inbox run we read the `To`, `Cc` and `Bcc` of every message in Sent.
//!
//! # Privacy and safety
//!
//! - Only the `ENVELOPE` and two header fields (`In-Reply-To`, `References`, fetched with
//!   `PEEK`) are read, never a body, and nothing on the server is changed.
//! - Only addresses are stored, in the `known_senders` table, and the Message-IDs the user
//!   answered, in `sent_replies`. The auto-clear pass uses those ids to clear "Reply needed"
//!   flags once the user has replied.
//!
//! # Incremental by UID
//!
//! The position in Sent is kept in `mailbox_state` as `(uidvalidity, last_uid)`. A run fetches
//! only UIDs above `last_uid`, in batches, and saves the position after every batch, so an
//! interrupted first run over a large archive resumes where it stopped. If the server changes
//! UIDVALIDITY the old UIDs mean nothing, so we start again from UID 1. That is safe because
//! known senders are only ever added, never removed.
//!
//! Unlike the inbox, the first run reads the **whole** history: an address you wrote to five
//! years ago is still someone you know. (Whether to cap this at the last N years is an open
//! question; there is no cap yet.)
//!
//! # Async and the database
//!
//! [`Store`] is synchronous, so each batch's writes run inside `tokio::task::spawn_blocking`
//! (see the store's module docs). The store is shared as `Arc<Mutex<Store>>`: `Arc` lets the
//! blocking closure own a handle to it, and the `Mutex` allows only one user at a time because
//! a rusqlite `Connection` is `Send` but not `Sync`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::AccountConfig;
use crate::imap::{ImapError, ImapSession, decode_modified_utf7};
use crate::store::{Store, StoreError};

/// How many UIDs one `UID FETCH` covers. Big enough to be fast, small enough that the
/// progress is saved often and no single response is huge.
pub const BATCH_SIZE: u32 = 500;

/// Why [`harvest_known_senders`] ended without reading the Sent folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// `policy.known_senders_from_sent` is `false`.
    Disabled,
    /// No Sent folder is configured and none could be detected.
    NoSentFolder,
}

/// What one harvest did. The counts let tests (and logs) prove that only new mail was read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HarvestReport {
    /// The Sent folder that was read, if any.
    pub folder: Option<String>,
    /// Set when nothing was read, with the reason.
    pub skipped: Option<Skipped>,
    /// True if UIDVALIDITY changed and the folder was rescanned from the start.
    pub rescanned: bool,
    /// Number of `UID FETCH` batches issued.
    pub batches: u32,
    /// Messages whose envelope was read.
    pub messages: u32,
    /// Addresses that were new to the store.
    pub new_senders: u32,
}

/// Everything that can go wrong while harvesting.
#[derive(Debug, thiserror::Error)]
pub enum HarvestError {
    /// Talking to the IMAP server failed.
    #[error(transparent)]
    Imap(#[from] ImapError),
    /// The database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The blocking database task panicked or was cancelled.
    #[error("the database task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// The inclusive UID ranges to fetch, given the last UID already harvested and the server's
/// `UIDNEXT`. Empty when there is nothing new.
///
/// `UIDNEXT` is one past the highest UID in use, so the last UID that can exist is
/// `uid_next - 1`. The ranges may include UIDs that were deleted; the server just skips them.
pub fn plan_batches(last_uid: u32, uid_next: u32, batch_size: u32) -> Vec<(u32, u32)> {
    let highest = uid_next.saturating_sub(1);
    let step = batch_size.max(1);
    let mut ranges = Vec::new();
    let mut first = last_uid.saturating_add(1);
    while first <= highest {
        // `saturating_*` keeps this safe even next to `u32::MAX`.
        let last = first.saturating_add(step - 1).min(highest);
        ranges.push((first, last));
        if last == u32::MAX {
            break;
        }
        first = last + 1;
    }
    ranges
}

/// Where to start: the saved position if UIDVALIDITY still matches, otherwise the beginning.
///
/// Returns `(last_uid, rescanned)`. `rescanned` is true only when there *was* a saved state
/// that is now invalid, not on the very first run.
pub fn resume_point(saved: Option<(u32, u32)>, current_uidvalidity: u32) -> (u32, bool) {
    match saved {
        Some((validity, last_uid)) if validity == current_uidvalidity => (last_uid, false),
        Some(_) => (0, true),
        None => (0, false),
    }
}

/// The addresses that identify the account itself. We do not "know" ourselves as a sender, so
/// they are not stored. Only the login name is available, and only if it looks like an email
/// address (some servers log in with a bare user name).
fn own_addresses(account: &AccountConfig) -> HashSet<String> {
    let username = account.username.trim().to_lowercase();
    if username.contains('@') {
        HashSet::from([username])
    } else {
        HashSet::new()
    }
}

// ---------------------------------------------------------------------------------------------
// The harvester
// ---------------------------------------------------------------------------------------------

/// Read new messages in the account's Sent folder and remember their recipients.
///
/// Uses the configured `sent_folder`, or the folder the server marks `\Sent`. Does nothing if
/// `known_senders_from_sent` is false. Leaves the session with the Sent folder selected, so
/// callers that go on to read the inbox must select it again.
///
/// `policy_enabled` is `config.policy.known_senders_from_sent`; it is a parameter, not read
/// from a whole `Config`, so the caller only needs the account's own settings.
pub async fn harvest_known_senders(
    session: &mut ImapSession,
    store: &Arc<Mutex<Store>>,
    account: &AccountConfig,
    policy_enabled: bool,
) -> Result<HarvestReport, HarvestError> {
    if !policy_enabled {
        return Ok(HarvestReport {
            skipped: Some(Skipped::Disabled),
            ..HarvestReport::default()
        });
    }

    let folder = if account.sent_folder.trim().is_empty() {
        session.resolve_folders().await?.sent
    } else {
        Some(account.sent_folder.clone())
    };
    let Some(folder) = folder else {
        tracing::warn!(account = %account.name, "no Sent folder found; known senders disabled");
        return Ok(HarvestReport {
            skipped: Some(Skipped::NoSentFolder),
            ..HarvestReport::default()
        });
    };

    let info = session.select(&folder).await?;
    let saved = with_store(store, {
        let (name, folder) = (account.name.clone(), folder.clone());
        move |store| store.mailbox_state(&name, &folder)
    })
    .await?
    .map(|state| (state.uidvalidity, state.last_uid));
    let (last_uid, rescanned) = resume_point(saved, info.uid_validity);

    let ranges = plan_batches(last_uid, info.uid_next, BATCH_SIZE);
    let mut report = HarvestReport {
        folder: Some(folder.clone()),
        rescanned,
        ..HarvestReport::default()
    };
    if rescanned {
        tracing::info!(
            account = %account.name, folder = %decode_modified_utf7(&folder),
            "UIDVALIDITY changed; rescanning Sent"
        );
    }
    tracing::info!(
        account = %account.name, folder = %decode_modified_utf7(&folder), batches = ranges.len(),
        "harvesting known senders"
    );

    let own = own_addresses(account);
    let total = ranges.len();
    for (index, (first, last)) in ranges.into_iter().enumerate() {
        let envelopes = session.fetch_envelopes(first, last).await?;

        // Deduplicate here so a thousand mails to the same person cost one insert.
        let addresses: HashSet<String> = envelopes
            .iter()
            .flat_map(|envelope| envelope.recipients.iter())
            .filter(|address| !own.contains(*address))
            .cloned()
            .collect();

        // The ids of messages the user answered, for clearing "Reply needed" flags.
        let reply_ids: Vec<String> = envelopes
            .iter()
            .flat_map(|envelope| envelope.reply_ids.iter())
            .collect::<HashSet<_>>()
            .into_iter()
            .cloned()
            .collect();

        let (name, mailbox, validity) = (account.name.clone(), folder.clone(), info.uid_validity);
        let new_senders = with_store(store, move |store| {
            store.add_reply_ids(&name, &reply_ids)?;
            let mut added = 0u32;
            for address in &addresses {
                if store.add_known_sender(&name, address)? {
                    added += 1;
                }
            }
            // Saved with the same call that notes the UIDVALIDITY, so a crash between batches
            // loses at most the batch in flight, and re-reading it is harmless.
            store.set_mailbox_state(&name, &mailbox, validity, last)?;
            Ok(added)
        })
        .await?;

        report.batches += 1;
        report.messages += u32::try_from(envelopes.len()).unwrap_or(u32::MAX);
        report.new_senders += new_senders;
        tracing::info!(
            account = %account.name, batch = index + 1, of = total, up_to_uid = last,
            new_senders, "Sent harvest progress"
        );
    }

    // Nothing to fetch, but still record the UIDVALIDITY on a first run (or after a reset), so
    // the next run knows what it is comparing against.
    if report.batches == 0 && (saved.is_none() || rescanned) {
        let (name, mailbox, validity) = (account.name.clone(), folder, info.uid_validity);
        with_store(store, move |store| {
            store.set_mailbox_state(&name, &mailbox, validity, 0)?;
            Ok(())
        })
        .await?;
    }

    tracing::info!(
        account = %account.name, messages = report.messages, new_senders = report.new_senders,
        "known-sender harvest finished"
    );
    Ok(report)
}

/// Run a synchronous store operation on tokio's blocking thread pool.
///
/// `work` must be `'static` because the thread outlives this call's borrows, which is why the
/// callers clone names into it. A poisoned lock (another thread panicked while holding it) is
/// recovered rather than propagated: SQLite rolls back a half-done transaction on its own, so
/// the data is still consistent.
pub(crate) async fn with_store<T, F>(store: &Arc<Mutex<Store>>, work: F) -> Result<T, HarvestError>
where
    T: Send + 'static,
    F: FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
{
    let store = Arc::clone(store);
    let result = tokio::task::spawn_blocking(move || {
        let mut guard = store.lock().unwrap_or_else(PoisonError::into_inner);
        work(&mut guard)
    })
    .await?;
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_nothing_when_up_to_date() {
        assert!(plan_batches(0, 1, 500).is_empty()); // empty folder: UIDNEXT is 1
        assert!(plan_batches(9, 10, 500).is_empty());
        assert!(plan_batches(20, 10, 500).is_empty()); // stored state ahead of the server
    }

    #[test]
    fn plans_one_short_batch() {
        assert_eq!(plan_batches(0, 4, 500), vec![(1, 3)]);
        assert_eq!(plan_batches(2, 6, 500), vec![(3, 5)]);
    }

    #[test]
    fn splits_into_batches_without_gaps_or_overlap() {
        assert_eq!(
            plan_batches(0, 1202, 500),
            vec![(1, 500), (501, 1000), (1001, 1201)]
        );
        assert_eq!(plan_batches(0, 1001, 500), vec![(1, 500), (501, 1000)]);
    }

    #[test]
    fn survives_the_top_of_the_uid_range() {
        assert_eq!(
            plan_batches(u32::MAX - 3, u32::MAX, 500),
            vec![(u32::MAX - 2, u32::MAX - 1)]
        );
        assert_eq!(plan_batches(u32::MAX, u32::MAX, 500), vec![]);
        // A zero batch size would loop forever without the `max(1)`.
        assert_eq!(plan_batches(0, 3, 0), vec![(1, 1), (2, 2)]);
    }

    #[test]
    fn resumes_only_when_uidvalidity_matches() {
        assert_eq!(resume_point(None, 7), (0, false));
        assert_eq!(resume_point(Some((7, 120)), 7), (120, false));
        assert_eq!(resume_point(Some((6, 120)), 7), (0, true));
    }

    #[test]
    fn own_address_comes_from_an_email_style_username() {
        let account = |username: &str| -> AccountConfig {
            let toml = format!("name = \"a\"\nhost = \"127.0.0.1\"\nusername = \"{username}\"\n");
            toml::from_str(&toml).unwrap()
        };
        assert_eq!(
            own_addresses(&account("Me@Example.org")),
            HashSet::from(["me@example.org".to_string()])
        );
        assert!(own_addresses(&account("me")).is_empty());
    }
}
