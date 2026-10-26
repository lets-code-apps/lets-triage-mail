//! Writing the digest: gather, render, then `APPEND` it to the mailbox or save it as a file.
//!
//! [`send_digest`] is the one entry point for both the schedule and the `digest` command. It
//! follows the account's mode (see [`AccountConfig::dry_run`]):
//!
//! - **Dry-run account:** nothing is ever appended. The digest is saved as a `.eml` file in
//!   the `digests` directory next to the database, so the owner can preview what the digest
//!   will look like (double-click the file to open it in Mail).
//! - **Any other account** (live, or classify-only without dry-run): the digest is appended to
//!   `[digest] folder`, unread.
//!
//! # Recognising our own digest
//!
//! The inbox worker must not classify the digest it finds in the inbox. It cannot go by the
//! `X-Lets-Triage-Mail` header, because a sender can write that header too and use it to slip
//! past the classifier. So every digest is recorded in the store ([`Store::begin_digest_message`]),
//! and the worker skips exactly those:
//!
//! 1. A random Message-ID is generated and stored **before** the `APPEND`. The worker may see
//!    the new message before we get the server's answer; this closes that gap. The part before
//!    the `@` is 128 random bits, so nobody can write a mail that carries it.
//! 2. The server's `APPENDUID` answer (UIDPLUS) gives the UID, which is stored too, so the
//!    worker can skip the message without downloading it.
//!
//! If the server gives no `APPENDUID`, step 1 alone does the job.
//!
//! # Hard rules
//!
//! This module appends mail and does nothing else to a mailbox: no `STORE`, no move, copy,
//! expunge or delete. A source-scan test enforces it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use jiff::Zoned;

use super::build::{DigestInput, build};
use super::render::{Envelope, Rendered, compose, render};
use crate::config::{AccountConfig, Config, ConfigError, FlagMode};
use crate::imap::worker::own_address;
use crate::imap::{ImapError, ImapSession};
use crate::store::{Store, StoreError};

/// The domain of our `Message-ID`s. `.invalid` is reserved (RFC 6761): it never resolves, so a
/// digest can never be mistaken for mail from a real host.
pub const MESSAGE_ID_DOMAIN: &str = "lets-triage-mail.invalid";

/// The `From` address when `[digest] from` is not set.
const DEFAULT_FROM_ADDRESS: &str = "lets-triage-mail@localhost";
const DEFAULT_FROM_NAME: &str = "Let's Triage Mail";

/// How far back a digest looks when there was no digest before.
const DEFAULT_WINDOW_SECONDS: i64 = 24 * 60 * 60;
/// How far back a digest looks at most, however long ago the last one was. After a long
/// sleep (a week away) a digest of the whole week would be a wall of text; the last two days
/// are what is still worth reading, and `stats` has the rest.
const MAX_WINDOW_SECONDS: i64 = 48 * 60 * 60;

/// Preview files kept per account; older ones are deleted.
pub const PREVIEWS_KEPT: usize = 14;

/// How long connecting and appending may take.
const IMAP_TIMEOUT: Duration = Duration::from_secs(60);

/// What happened to a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// It was appended to the mailbox.
    Appended {
        /// The folder it went into.
        folder: String,
        /// Its UID there, when the server said (UIDPLUS).
        uid: Option<u32>,
    },
    /// The account is in dry-run: it was written to this file instead.
    Saved(PathBuf),
}

/// Everything that can go wrong while writing a digest.
#[derive(Debug, thiserror::Error)]
pub enum DigestError {
    /// The database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The mail server failed or refused (for example, the folder does not exist).
    #[error("mail server: {0}")]
    Imap(#[from] ImapError),
    /// Writing the preview file failed, or building the message did.
    #[error("file or message error: {0}")]
    Io(#[from] io::Error),
    /// The password could not be read.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A blocking task panicked.
    #[error("internal error: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// The server did not answer in time.
    #[error("timed out talking to the mail server")]
    Timeout,
    /// The operating system gave no random numbers.
    #[error("cannot generate a random Message-ID: {0}")]
    Random(String),
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// The period a digest covers, as Unix seconds `(from, to)`.
///
/// It starts where the previous digest ended, so nothing is reported twice or missed, but not
/// earlier than [`MAX_WINDOW_SECONDS`] ago. Without a previous digest it is the last 24 hours.
pub fn digest_period(now: i64, last_digest_at: Option<i64>) -> (i64, i64) {
    let floor = now - MAX_WINDOW_SECONDS;
    let from = last_digest_at
        .unwrap_or(now - DEFAULT_WINDOW_SECONDS)
        .clamp(floor, now);
    (from, now)
}

/// A new, unguessable `Message-ID` (without angle brackets): 128 random bits in hex, an `@`
/// and [`MESSAGE_ID_DOMAIN`].
pub fn new_message_id() -> Result<String, DigestError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| DigestError::Random(e.to_string()))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{hex}@{MESSAGE_ID_DOMAIN}"))
}

/// Splits the `[digest] from` setting (`Name <address>` or a bare address) into name and
/// address. `None` gives the defaults.
pub fn parse_from(setting: Option<&str>) -> (String, String) {
    let default = || {
        (
            DEFAULT_FROM_NAME.to_owned(),
            DEFAULT_FROM_ADDRESS.to_owned(),
        )
    };
    let Some(text) = setting.map(str::trim).filter(|t| !t.is_empty()) else {
        return default();
    };
    if let Some((name, rest)) = text.split_once('<')
        && let Some((address, _)) = rest.split_once('>')
    {
        let name = name.trim().trim_matches('"').trim();
        let name = if name.is_empty() {
            DEFAULT_FROM_NAME
        } else {
            name
        };
        return (name.to_owned(), address.trim().to_owned());
    }
    (DEFAULT_FROM_NAME.to_owned(), text.to_owned())
}

/// A file-name-safe version of an account name: lowercase letters and digits, everything else
/// `-`. The account name comes from the config, but it ends up in a path, so no `/` or `..`
/// may survive.
pub fn account_slug(account: &str) -> String {
    let slug: String = account
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    if slug.is_empty() {
        "account".to_owned()
    } else {
        slug
    }
}

/// The file name of a preview: `<account-slug>-<YYYY-MM-DD>.eml`.
pub fn preview_file_name(account: &str, date: jiff::civil::Date) -> String {
    format!("{}-{date}.eml", account_slug(account))
}

/// Where preview files go: a `digests` directory next to the database.
pub fn previews_dir(config: &Config) -> PathBuf {
    config
        .db_path()
        .parent()
        .map_or_else(|| PathBuf::from("digests"), |dir| dir.join("digests"))
}

// ---------------------------------------------------------------------------------------------
// Preview files
// ---------------------------------------------------------------------------------------------

/// Saves a preview file (mode 0600: it lists who wrote to the user) and deletes this account's
/// older previews beyond `keep`. Returns the path.
pub fn save_preview(
    dir: &Path,
    account: &str,
    date: jiff::civil::Date,
    message: &[u8],
    keep: usize,
) -> io::Result<PathBuf> {
    create_private_dir(dir)?;
    let path = dir.join(preview_file_name(account, date));
    write_private_file(&path, message)?;
    prune_previews(dir, account, keep)?;
    Ok(path)
}

/// Deletes all but the newest `keep` previews of one account.
///
/// Only files named exactly `<slug>-<date>.eml` count, so another account whose slug starts
/// with this one (`work` and `work-old`) is never touched, and neither is anything else the
/// user keeps in the directory.
fn prune_previews(dir: &Path, account: &str, keep: usize) -> io::Result<()> {
    let prefix = format!("{}-", account_slug(account));
    let mut ours: Vec<(jiff::civil::Date, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let date = name
            .strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".eml"))
            .and_then(|date| date.parse::<jiff::civil::Date>().ok());
        if let Some(date) = date {
            ours.push((date, entry.path()));
        }
    }
    // Newest first; everything after the first `keep` goes.
    ours.sort_by_key(|(date, _)| std::cmp::Reverse(*date));
    for (_, path) in ours.into_iter().skip(keep) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_private_file(path: &Path, content: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // The mode above only applies when the file is created; make sure an older file that was
    // created more openly is tightened too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(content)
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, content: &[u8]) -> io::Result<()> {
    std::fs::write(path, content)
}

// ---------------------------------------------------------------------------------------------
// Building one digest from the store
// ---------------------------------------------------------------------------------------------

/// A digest ready to print, save or append.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub rendered: Rendered,
    /// The moment the digest covers up to (Unix seconds). Saved as `last_digest_at` after
    /// delivery, so the next digest starts exactly here.
    pub until: i64,
}

/// Runs a blocking store call on tokio's blocking pool. The store is shared as
/// `Arc<Mutex<Store>>`; the lock is held only inside the closure, never across an `.await`.
pub async fn with_store<T, F>(store: &Arc<Mutex<Store>>, work: F) -> Result<T, DigestError>
where
    T: Send + 'static,
    F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
{
    let store = Arc::clone(store);
    let result = tokio::task::spawn_blocking(move || {
        let guard = store.lock().unwrap_or_else(PoisonError::into_inner);
        work(&guard)
    })
    .await?;
    Ok(result?)
}

/// Reads the store and renders the digest for one account as of `now`. Changes nothing.
pub async fn prepare_digest(
    account: &AccountConfig,
    config: &Config,
    store: &Arc<Mutex<Store>>,
    now: &Zoned,
) -> Result<Prepared, DigestError> {
    let name = account.name.clone();
    let now_seconds = now.timestamp().as_second();
    let (period, rows) = with_store(store, move |s| {
        let period = digest_period(now_seconds, s.last_digest_at(&name)?);
        let rows = s.digest_rows(&name, period.0, period.1)?;
        Ok((period, rows))
    })
    .await?;

    // A classify-only account never chooses a flag, so ask the engine what it would choose.
    let policy = config.flag_policy();
    let derive_flags =
        (account.flag_mode(&config.policy) == FlagMode::ClassifyOnly).then_some(&policy);
    let digest = build(&DigestInput {
        account: &account.name,
        date: now.date(),
        period,
        rows: &rows,
        tz: now.time_zone(),
        flag_name: &|flag| config.flag_name(flag).to_owned(),
        derive_flags,
        dry_run: account.dry_run(&config.policy),
    });
    Ok(Prepared {
        rendered: render(&digest),
        until: period.1,
    })
}

// ---------------------------------------------------------------------------------------------
// Delivering it
// ---------------------------------------------------------------------------------------------

/// Builds this account's digest and delivers it: appended to the mailbox, or, for a dry-run
/// account, saved as a preview file. On success the day and moment are recorded, so the
/// schedule does not repeat it and the next digest starts where this one ended.
///
/// On failure nothing is recorded, so the schedule tries again.
pub async fn send_digest(
    account: &AccountConfig,
    config: &Config,
    store: &Arc<Mutex<Store>>,
    now: &Zoned,
) -> Result<Delivery, DigestError> {
    let prepared = prepare_digest(account, config, store, now).await?;

    let (from_name, from_address) = parse_from(config.digest.from.as_deref());
    let to = own_address(account).unwrap_or_else(|| from_address.clone());
    let message_id = new_message_id()?;
    let message = compose(
        &prepared.rendered,
        &Envelope {
            from_name: &from_name,
            from_address: &from_address,
            to_address: &to,
            message_id: &message_id,
            date: now.timestamp().as_second(),
        },
    )?;

    let delivery = if account.dry_run(&config.policy) {
        // Dry-run means the mailbox is not written to, and an APPEND is a write.
        let path = save_preview(
            &previews_dir(config),
            &account.name,
            now.date(),
            &message,
            PREVIEWS_KEPT,
        )?;
        Delivery::Saved(path)
    } else {
        append_to_mailbox(account, config, store, &message_id, &message).await?
    };

    let name = account.name.clone();
    let date = now.date().to_string();
    let until = prepared.until;
    with_store(store, move |s| s.set_last_digest(&name, &date, until)).await?;
    Ok(delivery)
}

/// Connects, appends and records the digest so the worker recognises it. Uses its own
/// short-lived connection: the inbox worker's connection is busy waiting in `IDLE`.
async fn append_to_mailbox(
    account: &AccountConfig,
    config: &Config,
    store: &Arc<Mutex<Store>>,
    message_id: &str,
    message: &[u8],
) -> Result<Delivery, DigestError> {
    let folder = config.digest.folder.clone();
    let name = account.name.clone();

    // First the record, then the APPEND: see the module docs.
    {
        let (name, id, folder) = (name.clone(), message_id.to_owned(), folder.clone());
        with_store(store, move |s| {
            s.begin_digest_message(&name, &id, &folder)?;
            // Records of earlier digests are useless after a few days.
            s.prune_own_digests(now_unix() - 7 * 24 * 60 * 60)?;
            Ok(())
        })
        .await?;
    }

    // `password_command` runs a shell command and waits, so it goes to a blocking thread.
    let for_password = account.clone();
    let password = tokio::task::spawn_blocking(move || for_password.password()).await??;

    let appended = tokio::time::timeout(IMAP_TIMEOUT, async {
        let mut session = ImapSession::connect(&account.imap_settings(&password)).await?;
        let result = session.append_message(&folder, message).await;
        // Best effort: the connection is dropped either way.
        let _ = session.logout().await;
        result
    })
    .await
    .map_err(|_elapsed| DigestError::Timeout)??;

    if let (Some(uid_validity), Some(uid)) = (appended.uid_validity, appended.uid) {
        let id = message_id.to_owned();
        with_store(store, move |s| {
            s.set_digest_uid(&name, &id, uid_validity, uid).map(drop)
        })
        .await?;
    } else {
        // No APPENDUID: the Message-ID record alone identifies the digest.
        tracing::debug!(account = %account.name, "the server gave no APPENDUID; the worker will match the Message-ID");
    }
    Ok(Delivery::Appended {
        folder: config.digest.folder.clone(),
        uid: appended.uid,
    })
}

fn now_unix() -> i64 {
    jiff::Timestamp::now().as_second()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_period_starts_where_the_last_digest_ended() {
        let now = 1_000_000;
        assert_eq!(digest_period(now, Some(now - 3_600)), (now - 3_600, now));
    }

    #[test]
    fn the_first_digest_looks_back_a_day() {
        let now = 1_000_000;
        assert_eq!(digest_period(now, None), (now - 86_400, now));
    }

    #[test]
    fn a_long_sleep_is_capped_at_two_days() {
        let now = 10_000_000;
        assert_eq!(
            digest_period(now, Some(now - 7 * 86_400)),
            (now - 2 * 86_400, now)
        );
    }

    #[test]
    fn a_last_digest_in_the_future_is_not_used() {
        // The clock was set back: never a period that ends before it starts.
        let now = 1_000_000;
        assert_eq!(digest_period(now, Some(now + 500)), (now, now));
    }

    #[test]
    fn message_ids_are_long_random_and_ours() {
        let a = new_message_id().unwrap();
        let b = new_message_id().unwrap();
        assert_ne!(a, b);
        let (local, domain) = a.split_once('@').unwrap();
        assert_eq!(domain, MESSAGE_ID_DOMAIN);
        assert_eq!(local.len(), 32);
        assert!(local.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn the_from_setting_is_split_into_name_and_address() {
        let (name, address) = parse_from(Some("Triage <triage@example.com>"));
        assert_eq!(
            (name.as_str(), address.as_str()),
            ("Triage", "triage@example.com")
        );
        let (name, address) = parse_from(Some("\"Quoted Name\" <q@example.com>"));
        assert_eq!(
            (name.as_str(), address.as_str()),
            ("Quoted Name", "q@example.com")
        );
        let (name, address) = parse_from(Some("bare@example.com"));
        assert_eq!(
            (name.as_str(), address.as_str()),
            (DEFAULT_FROM_NAME, "bare@example.com")
        );
        let (name, address) = parse_from(Some("<only@example.com>"));
        assert_eq!(
            (name.as_str(), address.as_str()),
            (DEFAULT_FROM_NAME, "only@example.com")
        );
        assert_eq!(
            parse_from(None),
            (
                DEFAULT_FROM_NAME.to_owned(),
                DEFAULT_FROM_ADDRESS.to_owned()
            )
        );
        assert_eq!(parse_from(Some("  ")), parse_from(None));
    }

    #[test]
    fn account_names_cannot_escape_the_directory() {
        assert_eq!(account_slug("Work Mail"), "work-mail");
        assert_eq!(account_slug("../../etc/passwd"), "------etc-passwd");
        assert_eq!(account_slug(""), "account");
        assert_eq!(
            preview_file_name("wp.pl", jiff::civil::date(2026, 10, 26)),
            "wp-pl-2026-10-26.eml"
        );
    }

    #[test]
    fn previews_are_private_and_only_the_newest_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("digests");
        let date = |day: i8| jiff::civil::date(2026, 10, day);

        for day in 1..=5 {
            save_preview(&dir, "work", date(day), b"x", 3).unwrap();
        }
        // Another account whose slug starts with ours, and an unrelated file: both untouched.
        save_preview(&dir, "work-old", date(1), b"x", 3).unwrap();
        std::fs::write(dir.join("notes.txt"), "keep me").unwrap();
        save_preview(&dir, "work", date(6), b"x", 3).unwrap();

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "notes.txt",
                "work-2026-10-04.eml",
                "work-2026-10-05.eml",
                "work-2026-10-06.eml",
                "work-old-2026-10-01.eml",
            ]
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("work-2026-10-06.eml")), 0o600);
        }
    }

    #[test]
    fn saving_again_replaces_the_file_of_the_same_day() {
        let dir = tempfile::tempdir().unwrap();
        let date = jiff::civil::date(2026, 10, 26);
        save_preview(dir.path(), "a", date, b"first, longer content", 14).unwrap();
        let path = save_preview(dir.path(), "a", date, b"second", 14).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"second");
    }

    // ---- the whole flow for a dry-run account (no server needed) --------------------------

    use crate::engine::{Category, Payment, TriageFlag, Verdict};
    use crate::store::{MessageKey, ProcessedRecord};

    /// A dry-run account (the config default) and a config whose data directory is `dir`.
    fn dry_run_setup(dir: &Path) -> (AccountConfig, Config, Arc<Mutex<Store>>) {
        let config = Config::parse(&format!(
            "[storage]\ndb_path = \"{}\"\n",
            dir.join("state.db").display()
        ))
        .unwrap();
        let account: AccountConfig = toml::from_str(
            "name = \"Work\"\nhost = \"127.0.0.1\"\nusername = \"me@example.org\"\ntls = \"none\"\n",
        )
        .unwrap();
        assert!(account.dry_run(&config.policy));
        (
            account,
            config,
            Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
        )
    }

    fn record(store: &Store, uid: u32, subject: &str, flag: Option<TriageFlag>) {
        store
            .record_processed(&ProcessedRecord {
                key: MessageKey {
                    account: "Work".into(),
                    mailbox: "INBOX".into(),
                    uidvalidity: 1,
                    uid,
                },
                message_id: None,
                sender: Some("alice@example.org".into()),
                subject: Some(subject.into()),
                verdict: Some(Verdict {
                    category: Category::Personal,
                    needs_reply: true,
                    deadline: None,
                    payment: Payment::NotApplicable,
                    confidence: 0.9,
                    reason: "Asks you a question.".into(),
                }),
                flag,
                source: "model".into(),
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_dry_run_account_gets_a_preview_file_and_the_day_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let (account, config, store) = dry_run_setup(dir.path());
        record(
            &store.lock().unwrap(),
            1,
            "<script>alert(1)</script> Lunch?",
            Some(TriageFlag::ReplyNeeded),
        );
        let now = jiff::Zoned::now();

        let delivery = send_digest(&account, &config, &store, &now).await.unwrap();

        let Delivery::Saved(path) = delivery else {
            panic!("a dry-run account must never be appended to: {delivery:?}");
        };
        assert_eq!(path.parent().unwrap(), dir.path().join("digests"));
        let saved = std::fs::read(&path).unwrap();
        let message = mail_parser::MessageParser::default().parse(&saved).unwrap();
        let text = message.body_text(0).unwrap();
        assert!(text.contains("Reply needed (1)"), "{text}");
        assert!(text.contains("alice@example.org"), "{text}");
        assert!(text.contains("Dry-run: nothing was flagged"), "{text}");
        let html = message.body_html(0).unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");

        // Recorded, so the schedule does not write it again today, and no digest record for
        // the worker: nothing was appended.
        let guard = store.lock().unwrap();
        assert_eq!(
            guard.last_digest_date("Work").unwrap(),
            Some(now.date().to_string())
        );
        assert!(guard.last_digest_at("Work").unwrap().is_some());
    }

    #[tokio::test]
    async fn preparing_a_digest_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (account, config, store) = dry_run_setup(dir.path());
        record(
            &store.lock().unwrap(),
            1,
            "hello",
            Some(TriageFlag::ReplyNeeded),
        );

        prepare_digest(&account, &config, &store, &jiff::Zoned::now())
            .await
            .unwrap();

        assert_eq!(
            store.lock().unwrap().last_digest_date("Work").unwrap(),
            None
        );
        assert!(!dir.path().join("digests").exists());
    }

    // ---- the live path, against the scripted fake server ------------------------------------

    use crate::imap::fake_server::{AppendReply, fake_server};

    /// A live account (`dry_run = false`) that talks to the fake server on `port`.
    fn live_account(port: u16) -> AccountConfig {
        toml::from_str(&format!(
            "name = \"Work\"\nhost = \"127.0.0.1\"\nport = {port}\nusername = \"me@example.org\"\n\
             tls = \"none\"\npassword_command = \"echo pass\"\ndry_run = false\n"
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn a_live_account_appends_and_records_the_digest_by_uid_and_message_id() {
        let dir = tempfile::tempdir().unwrap();
        let (_, config, store) = dry_run_setup(dir.path());
        let (port, received) = fake_server(AppendReply::WithUid).await;
        let account = live_account(port);
        record(
            &store.lock().unwrap(),
            1,
            "Lunch?",
            Some(TriageFlag::ReplyNeeded),
        );

        let delivery = send_digest(&account, &config, &store, &jiff::Zoned::now())
            .await
            .unwrap();
        assert_eq!(
            delivery,
            Delivery::Appended {
                folder: "INBOX".into(),
                uid: Some(12)
            }
        );

        // What the server received is our digest, with a Message-ID that the store knows.
        let sent = received.await.unwrap();
        let message = mail_parser::MessageParser::default().parse(&sent).unwrap();
        let message_id = message.message_id().unwrap().to_owned();
        assert!(message_id.ends_with(&format!("@{MESSAGE_ID_DOMAIN}")));
        assert!(message.body_text(0).unwrap().contains("Lunch?"));
        assert!(
            !message.body_text(0).unwrap().contains("Dry-run"),
            "a live account's digest is not a preview"
        );

        let guard = store.lock().unwrap();
        assert!(guard.is_own_digest_message_id("Work", &message_id).unwrap());
        // APPENDUID said validity 77, UID 12.
        let key = MessageKey {
            account: "Work".into(),
            mailbox: "INBOX".into(),
            uidvalidity: 77,
            uid: 12,
        };
        assert!(guard.is_own_digest(&key).unwrap());
        assert!(guard.last_digest_date("Work").unwrap().is_some());
        // No file: a live account is not previewed.
        assert!(!dir.path().join("digests").exists());
    }

    #[tokio::test]
    async fn without_appenduid_the_digest_is_still_recognised_by_message_id() {
        let dir = tempfile::tempdir().unwrap();
        let (_, config, store) = dry_run_setup(dir.path());
        let (port, received) = fake_server(AppendReply::WithoutUid).await;

        let delivery = send_digest(&live_account(port), &config, &store, &jiff::Zoned::now())
            .await
            .unwrap();
        assert_eq!(
            delivery,
            Delivery::Appended {
                folder: "INBOX".into(),
                uid: None
            }
        );
        let sent = received.await.unwrap();
        let message = mail_parser::MessageParser::default().parse(&sent).unwrap();
        let guard = store.lock().unwrap();
        assert!(
            guard
                .is_own_digest_message_id("Work", message.message_id().unwrap())
                .unwrap()
        );
        assert!(guard.last_digest_date("Work").unwrap().is_some());
    }

    #[tokio::test]
    async fn a_refused_append_records_no_day_so_the_schedule_retries() {
        let dir = tempfile::tempdir().unwrap();
        let (_, config, store) = dry_run_setup(dir.path());
        let (port, _) = fake_server(AppendReply::NoSuchFolder).await;

        let error = send_digest(&live_account(port), &config, &store, &jiff::Zoned::now())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no such folder"), "{error}");
        assert_eq!(
            store.lock().unwrap().last_digest_date("Work").unwrap(),
            None
        );
    }

    /// Hard rule: the digest only appends mail. It never changes flags and never moves,
    /// copies, expunges or deletes anything. The forbidden words are assembled at runtime so
    /// this test does not match itself.
    #[test]
    fn digest_source_only_appends() {
        for (file, source) in [
            ("build.rs", include_str!("build.rs")),
            ("deliver.rs", include_str!("deliver.rs")),
            ("render.rs", include_str!("render.rs")),
            ("schedule.rs", include_str!("schedule.rs")),
            ("mod.rs", include_str!("mod.rs")),
        ] {
            let production_code = source.split("#[cfg(test)]").next().unwrap();
            for forbidden in [
                ["uid", "_store"].concat(),
                [".store", "_flags("].concat(),
                [".exp", "unge("].concat(),
                [".uid", "_expunge("].concat(),
                [".uid", "_mv("].concat(),
                [".mv", "("].concat(),
                [".uid", "_copy("].concat(),
                [".copy", "("].concat(),
                [".del", "ete("].concat(),
                ["\\Del", "eted"].concat(),
                ["\\Fl", "agged"].concat(),
                ["BODY", "["].concat(),
            ] {
                assert!(
                    !production_code.contains(&forbidden),
                    "{file}: the digest code must not use {forbidden}"
                );
            }
        }
    }
}
