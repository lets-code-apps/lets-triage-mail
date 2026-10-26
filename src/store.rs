//! SQLite persistence: everything the service must remember between runs.
//!
//! The store holds **metadata only**. Message bodies and passwords never enter the
//! database (see `CLAUDE.md`); the `labels` table keeps small evidence features, not text.
//!
//! # Async callers
//!
//! `rusqlite` is synchronous: a query blocks the thread that runs it. [`Store`] is
//! therefore a plain synchronous API. Code running inside a tokio task must not call it
//! directly on the async runtime; wrap the call in `tokio::task::spawn_blocking`, which
//! moves it to a thread pool meant for blocking work. (`Store` holds a `Connection`, which
//! is `Send` but not `Sync`, so share it as `Arc<Mutex<Store>>` or give each blocking task
//! its own `Store::open`. SQLite in WAL mode handles several connections well.)
//!
//! # Schema
//!
//! Migrations are an ordered list of SQL scripts in [`MIGRATIONS`]. SQLite keeps the number
//! of applied scripts in `PRAGMA user_version`, so opening a database applies exactly the
//! scripts it has not seen yet, each inside its own transaction. Never edit an old
//! script that has shipped: append a new one.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::engine::{Category, Payment, Rule, TriageFlag, Verdict};
use crate::flags::FlagColour;

/// Account name used for rules that apply to every account.
pub const GLOBAL_ACCOUNT: &str = "*";

/// Longest subject we keep. Subjects are metadata, but there is no reason to store more.
const MAX_SUBJECT_CHARS: usize = 300;
/// Longest reason or error text we keep.
const MAX_REASON_CHARS: usize = 500;

/// The schema, one script per version. Index 0 takes a database from version 0 to 1.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE mailbox_state (
        account     TEXT NOT NULL,
        mailbox     TEXT NOT NULL,
        uidvalidity INTEGER NOT NULL,
        last_uid    INTEGER NOT NULL,
        PRIMARY KEY (account, mailbox)
    );

    -- One row per message we looked at. No body column, on purpose.
    CREATE TABLE processed (
        id          INTEGER PRIMARY KEY,
        account     TEXT NOT NULL,
        mailbox     TEXT NOT NULL,
        uidvalidity INTEGER NOT NULL,
        uid         INTEGER NOT NULL,
        message_id  TEXT,
        sender      TEXT,
        subject     TEXT,
        category    TEXT,
        needs_reply INTEGER,
        deadline    TEXT,
        payment     TEXT,
        confidence  REAL,
        flag        TEXT,
        reason      TEXT,
        source      TEXT NOT NULL,
        created_at  INTEGER NOT NULL,
        UNIQUE (account, mailbox, uidvalidity, uid)
    );
    CREATE INDEX processed_created ON processed (created_at);
    CREATE INDEX processed_message_id ON processed (message_id);

    -- Flags the service set. A row with user_owned = 1 is a tombstone: the user changed
    -- the flag, so we must never touch that message again.
    CREATE TABLE flag_ownership (
        account     TEXT NOT NULL,
        mailbox     TEXT NOT NULL,
        uidvalidity INTEGER NOT NULL,
        uid         INTEGER NOT NULL,
        message_id  TEXT,
        colour      TEXT NOT NULL,
        reason      TEXT,
        set_at      INTEGER NOT NULL,
        user_owned  INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (account, mailbox, uidvalidity, uid)
    );

    CREATE TABLE rules (
        account    TEXT NOT NULL DEFAULT '*',
        pattern    TEXT NOT NULL,
        kind       TEXT NOT NULL CHECK (kind IN ('allow', 'block')),
        created_at INTEGER NOT NULL,
        PRIMARY KEY (account, pattern)
    );

    CREATE TABLE known_senders (
        account TEXT NOT NULL,
        address TEXT NOT NULL,
        domain  TEXT NOT NULL,
        PRIMARY KEY (account, address)
    );
    CREATE INDEX known_senders_domain ON known_senders (account, domain);

    CREATE TABLE retry_queue (
        id              INTEGER PRIMARY KEY,
        account         TEXT NOT NULL,
        mailbox         TEXT NOT NULL,
        uidvalidity     INTEGER NOT NULL,
        uid             INTEGER NOT NULL,
        attempts        INTEGER NOT NULL,
        next_attempt_at INTEGER NOT NULL,
        last_error      TEXT,
        UNIQUE (account, mailbox, uidvalidity, uid)
    );

    CREATE TABLE digest_state (
        account          TEXT PRIMARY KEY,
        last_digest_date TEXT NOT NULL
    );

    -- The future training set: evidence features plus the verdict, and later the
    -- correction the user made.
    CREATE TABLE labels (
        id              INTEGER PRIMARY KEY,
        account         TEXT NOT NULL,
        mailbox         TEXT NOT NULL,
        uidvalidity     INTEGER NOT NULL,
        uid             INTEGER NOT NULL,
        features        TEXT NOT NULL,
        category        TEXT NOT NULL,
        needs_reply     INTEGER NOT NULL,
        deadline        TEXT,
        payment         TEXT NOT NULL,
        confidence      REAL NOT NULL,
        flag            TEXT,
        reason          TEXT,
        created_at      INTEGER NOT NULL,
        user_correction TEXT,
        corrected_at    INTEGER,
        UNIQUE (account, mailbox, uidvalidity, uid)
    );
    ",
    // Version 2 (phase 4): what the auto-clear pass needs to know about a flag we set.
    "
    -- Which flag it was (the colour alone is configurable, so it cannot tell \"Reply needed\"
    -- from \"Deadline\"), and the date a Deadline flag expires on (ISO text).
    ALTER TABLE flag_ownership ADD COLUMN flag TEXT;
    ALTER TABLE flag_ownership ADD COLUMN deadline TEXT;

    -- Message-IDs the user answered, read from the In-Reply-To and References headers of
    -- the Sent folder. Ids only: no subject, no address, no body.
    CREATE TABLE sent_replies (
        account    TEXT NOT NULL,
        message_id TEXT NOT NULL,
        PRIMARY KEY (account, message_id)
    ) WITHOUT ROWID;
    ",
];

/// Everything that can go wrong in the store.
///
/// A library crate defines its own error type with `thiserror` so callers can match on it;
/// the binary then wraps it in `anyhow`.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite reported an error. `#[from]` lets `?` convert it automatically.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Creating the database directory or file failed.
    #[error("cannot prepare the database location: {0}")]
    Io(#[from] std::io::Error),
    /// The database was written by a newer version of this program.
    #[error("database schema version {found} is newer than this program supports ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },
    /// A text value in the database is not one we write. Corruption or a manual edit.
    #[error("unknown {kind} value in the database: {value:?}")]
    UnknownValue { kind: &'static str, value: String },
    /// A rule pattern or sender must be an address or `@domain`.
    #[error("invalid address or pattern: {0:?}")]
    InvalidPattern(String),
}

/// Shorthand used by every method in this module.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Identifies one message on the server. UIDs are only meaningful together with the
/// mailbox's UIDVALIDITY, so all three travel together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageKey {
    pub account: String,
    pub mailbox: String,
    pub uidvalidity: u32,
    pub uid: u32,
}

/// The saved position in one mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxState {
    pub uidvalidity: u32,
    pub last_uid: u32,
}

/// One processed message, to be logged. There is deliberately no body field.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessedRecord {
    pub key: MessageKey,
    pub message_id: Option<String>,
    pub sender: Option<String>,
    pub subject: Option<String>,
    /// `None` when the model was not asked (blocked sender) or failed.
    pub verdict: Option<Verdict>,
    /// The flag chosen for the message, if any.
    pub flag: Option<TriageFlag>,
    /// Who decided, for example `"model"` or `"rule"`.
    pub source: String,
}

/// A flag the service set on a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedFlag {
    pub key: MessageKey,
    pub message_id: Option<String>,
    pub colour: FlagColour,
    /// Which flag this was. `None` for a record that predates the column; such a flag is
    /// watched for user changes but never auto-cleared.
    pub flag: Option<TriageFlag>,
    /// The date a "Deadline" flag expires on. `None` for every other flag.
    pub deadline: Option<jiff::civil::Date>,
    pub reason: Option<String>,
    /// Unix seconds.
    pub set_at: i64,
}

/// A user rule as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleEntry {
    /// An account name, or [`GLOBAL_ACCOUNT`].
    pub account: String,
    pub pattern: String,
    pub rule: Rule,
}

/// A message waiting for another classification attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryItem {
    pub id: i64,
    pub key: MessageKey,
    /// How many attempts have failed so far.
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
}

/// One row of the labelled dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelRecord {
    pub key: MessageKey,
    /// Evidence features as JSON text (header facts, counts, flags). Never body text.
    pub features: String,
    pub verdict: Verdict,
    pub flag: Option<TriageFlag>,
}

/// A count for one label (a category or a flag) within one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountCount {
    pub account: String,
    /// The stable stored text, for example `"newsletter"` or `"to_pay"`.
    pub label: String,
    pub count: u64,
}

/// A count for one label, with no account attached (for example one sender).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counted {
    pub label: String,
    pub count: u64,
}

/// The state of one account's retry queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrySummary {
    pub account: String,
    /// Messages waiting for another model attempt.
    pub queued: u64,
    /// Of those, how many are due now.
    pub due: u64,
}

/// The SQLite-backed store. See the module docs for how to use it from async code.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating if needed) the database file and brings the schema up to date.
    ///
    /// On Unix the directory is created with mode 0700 and the file with 0600, because it
    /// holds who writes to the user.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir)?;
        }
        // Create the file ourselves first, with the right mode from the start. Letting
        // SQLite create it would leave a window where it has the default (umask) mode.
        create_private_file(path)?;
        Store::from_connection(Connection::open(path)?)
    }

    /// Opens a throwaway in-memory database. Used by tests.
    pub fn open_in_memory() -> Result<Store> {
        Store::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Store> {
        // `journal_mode` returns a row (the mode now in effect), so it is read, not executed.
        // In-memory databases stay in "memory" mode; that is fine.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Store { conn })
    }

    // ---- mailbox state ------------------------------------------------------------

    /// The saved position in a mailbox, or `None` the first time we see it.
    pub fn mailbox_state(&self, account: &str, mailbox: &str) -> Result<Option<MailboxState>> {
        let row = self
            .conn
            .query_row(
                "SELECT uidvalidity, last_uid FROM mailbox_state WHERE account = ?1 AND mailbox = ?2",
                params![account, mailbox],
                |row| {
                    Ok(MailboxState {
                        uidvalidity: row.get(0)?,
                        last_uid: row.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Saves the position in a mailbox.
    ///
    /// If the stored UIDVALIDITY differs, the server has renumbered the mailbox, so every
    /// UID we remember for it is meaningless. The flag-ownership and retry rows for the old
    /// numbering are deleted (acting on them could touch the wrong message). The `processed`
    /// and `labels` history stays, since it carries its own UIDVALIDITY.
    ///
    /// Returns `true` when such a reset happened.
    pub fn set_mailbox_state(
        &mut self,
        account: &str,
        mailbox: &str,
        uidvalidity: u32,
        last_uid: u32,
    ) -> Result<bool> {
        let tx = self.conn.transaction()?;
        let old: Option<u32> = tx
            .query_row(
                "SELECT uidvalidity FROM mailbox_state WHERE account = ?1 AND mailbox = ?2",
                params![account, mailbox],
                |row| row.get(0),
            )
            .optional()?;
        let reset = old.is_some_and(|old| old != uidvalidity);
        if reset {
            tx.execute(
                "DELETE FROM flag_ownership WHERE account = ?1 AND mailbox = ?2 AND uidvalidity != ?3",
                params![account, mailbox, uidvalidity],
            )?;
            tx.execute(
                "DELETE FROM retry_queue WHERE account = ?1 AND mailbox = ?2 AND uidvalidity != ?3",
                params![account, mailbox, uidvalidity],
            )?;
        }
        tx.execute(
            "INSERT INTO mailbox_state (account, mailbox, uidvalidity, last_uid)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account, mailbox)
             DO UPDATE SET uidvalidity = excluded.uidvalidity, last_uid = excluded.last_uid",
            params![account, mailbox, uidvalidity, last_uid],
        )?;
        tx.commit()?;
        Ok(reset)
    }

    // ---- processed log ------------------------------------------------------------

    /// Logs a processed message, replacing an earlier row for the same message.
    pub fn record_processed(&self, record: &ProcessedRecord) -> Result<()> {
        let v = record.verdict.as_ref();
        self.conn.execute(
            "INSERT OR REPLACE INTO processed
               (account, mailbox, uidvalidity, uid, message_id, sender, subject,
                category, needs_reply, deadline, payment, confidence, flag, reason,
                source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                record.key.account,
                record.key.mailbox,
                record.key.uidvalidity,
                record.key.uid,
                record.message_id,
                record.sender,
                record
                    .subject
                    .as_deref()
                    .map(|s| truncate(s, MAX_SUBJECT_CHARS)),
                v.map(|v| category_to_str(v.category)),
                v.map(|v| v.needs_reply),
                v.and_then(|v| v.deadline).map(|date| date.to_string()),
                v.map(|v| payment_to_str(v.payment)),
                v.map(|v| v.confidence),
                record.flag.map(flag_to_str),
                v.map(|v| truncate(&v.reason, MAX_REASON_CHARS)),
                record.source,
                now_unix(),
            ],
        )?;
        Ok(())
    }

    /// Whether this exact message (same UIDVALIDITY and UID) was already processed.
    pub fn is_processed(&self, key: &MessageKey) -> Result<bool> {
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM processed
                 WHERE account = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
                params![key.account, key.mailbox, key.uidvalidity, key.uid],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Who decided a processed message (`"model"` or `"rule"`), or `None` if it was not
    /// processed.
    pub fn processed_source(&self, key: &MessageKey) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT source FROM processed
                 WHERE account = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
                params![key.account, key.mailbox, key.uidvalidity, key.uid],
                |row| row.get(0),
            )
            .optional()?)
    }

    // ---- rules --------------------------------------------------------------------

    /// Adds a rule, or changes the kind of an existing one. `pattern` is an address or
    /// `@domain`; it is trimmed and lowercased. Use [`GLOBAL_ACCOUNT`] for all accounts.
    pub fn add_rule(&self, account: &str, pattern: &str, rule: Rule) -> Result<()> {
        let pattern = normalise_pattern(pattern)?;
        self.conn.execute(
            "INSERT INTO rules (account, pattern, kind, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account, pattern) DO UPDATE SET kind = excluded.kind",
            params![account, pattern, rule_to_str(rule), now_unix()],
        )?;
        Ok(())
    }

    /// Removes a rule. Returns whether one existed.
    pub fn remove_rule(&self, account: &str, pattern: &str) -> Result<bool> {
        let pattern = normalise_pattern(pattern)?;
        let removed = self.conn.execute(
            "DELETE FROM rules WHERE account = ?1 AND pattern = ?2",
            params![account, pattern],
        )?;
        Ok(removed > 0)
    }

    /// All rules, blocks first, then by pattern.
    pub fn list_rules(&self) -> Result<Vec<RuleEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT account, pattern, kind FROM rules ORDER BY kind, pattern, account")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut rules = Vec::new();
        for row in rows {
            let (account, pattern, kind) = row?;
            rules.push(RuleEntry {
                account,
                pattern,
                rule: rule_from_str(&kind)?,
            });
        }
        Ok(rules)
    }

    /// The rule that applies to a sender, if any.
    ///
    /// An exact address beats its `@domain`; for the same pattern, a rule for this account
    /// beats a global one. Matching ignores case.
    pub fn match_rule(&self, account: &str, sender: &str) -> Result<Option<Rule>> {
        let address = sender.trim().to_lowercase();
        let domain_pattern = match address.rsplit_once('@') {
            Some((_, domain)) => format!("@{domain}"),
            None => String::new(),
        };
        let kind: Option<String> = self
            .conn
            .query_row(
                "SELECT kind FROM rules
                 WHERE account IN (?1, '*') AND pattern IN (?2, ?3)
                 ORDER BY (pattern = ?2) DESC, (account = ?1) DESC
                 LIMIT 1",
                params![account, address, domain_pattern],
                |row| row.get(0),
            )
            .optional()?;
        kind.as_deref().map(rule_from_str).transpose()
    }

    // ---- known senders ------------------------------------------------------------

    /// Remembers an address the user has written to. Returns whether it was new.
    pub fn add_known_sender(&self, account: &str, address: &str) -> Result<bool> {
        let address = address.trim().to_lowercase();
        let Some((_, domain)) = address.rsplit_once('@') else {
            return Err(StoreError::InvalidPattern(address));
        };
        let added = self.conn.execute(
            "INSERT OR IGNORE INTO known_senders (account, address, domain) VALUES (?1, ?2, ?3)",
            params![account, address, domain],
        )?;
        Ok(added > 0)
    }

    /// Whether the user has written to this address.
    pub fn is_known_sender(&self, account: &str, address: &str) -> Result<bool> {
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM known_senders WHERE account = ?1 AND address = ?2",
                params![account, address.trim().to_lowercase()],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// How many known addresses share a domain; a hint that the domain is trusted.
    pub fn known_domain_count(&self, account: &str, domain: &str) -> Result<u32> {
        let domain = domain.trim().trim_start_matches('@').to_lowercase();
        let count = self.conn.query_row(
            "SELECT COUNT(*) FROM known_senders WHERE account = ?1 AND domain = ?2",
            params![account, domain],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    // ---- flag ownership -----------------------------------------------------------

    /// Records that we set a flag. Call it in the same step as the IMAP `STORE`.
    ///
    /// A message the user has taken over (`user_owned`) is never overwritten.
    pub fn record_flag(&self, flag: &OwnedFlag) -> Result<()> {
        self.conn.execute(
            "INSERT INTO flag_ownership
               (account, mailbox, uidvalidity, uid, message_id, colour, flag, deadline,
                reason, set_at, user_owned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)
             ON CONFLICT (account, mailbox, uidvalidity, uid) DO UPDATE SET
               message_id = excluded.message_id, colour = excluded.colour,
               flag = excluded.flag, deadline = excluded.deadline,
               reason = excluded.reason, set_at = excluded.set_at
             WHERE flag_ownership.user_owned = 0",
            params![
                flag.key.account,
                flag.key.mailbox,
                flag.key.uidvalidity,
                flag.key.uid,
                flag.message_id,
                colour_to_str(flag.colour),
                flag.flag.map(flag_to_str),
                flag.deadline.map(|date| date.to_string()),
                flag.reason
                    .as_deref()
                    .map(|s| truncate(s, MAX_REASON_CHARS)),
                flag.set_at,
            ],
        )?;
        Ok(())
    }

    /// Flags we set in a mailbox and may still clear. Messages the user took over are
    /// never returned, so callers cannot clear them by accident. Oldest first.
    pub fn owned_flags(&self, account: &str, mailbox: &str) -> Result<Vec<OwnedFlag>> {
        let mut stmt = self.conn.prepare(
            "SELECT uidvalidity, uid, message_id, colour, reason, set_at, flag, deadline
             FROM flag_ownership
             WHERE account = ?1 AND mailbox = ?2 AND user_owned = 0
             ORDER BY set_at, uid",
        )?;
        let rows = stmt.query_map(params![account, mailbox], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        let mut flags = Vec::new();
        for row in rows {
            let (uidvalidity, uid, message_id, colour, reason, set_at, flag, deadline) = row?;
            flags.push(OwnedFlag {
                key: MessageKey {
                    account: account.to_owned(),
                    mailbox: mailbox.to_owned(),
                    uidvalidity,
                    uid,
                },
                message_id,
                colour: colour_from_str(&colour)?,
                flag: flag.as_deref().map(flag_from_str).transpose()?,
                // A date we cannot read back is treated as no date, which means "never
                // auto-clear": the safe direction.
                deadline: deadline.and_then(|text| text.parse().ok()),
                reason,
                set_at,
            });
        }
        Ok(flags)
    }

    /// Marks a message as user-owned: the user changed its flag, so we leave it alone
    /// forever. The row stays as a tombstone. Returns whether we had a record for it.
    pub fn mark_user_owned(&self, key: &MessageKey) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE flag_ownership SET user_owned = 1
             WHERE account = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
            params![key.account, key.mailbox, key.uidvalidity, key.uid],
        )?;
        Ok(changed > 0)
    }

    /// Whether the user has taken this message's flag over.
    pub fn is_user_owned(&self, key: &MessageKey) -> Result<bool> {
        let owned: Option<bool> = self
            .conn
            .query_row(
                "SELECT user_owned FROM flag_ownership
                 WHERE account = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
                params![key.account, key.mailbox, key.uidvalidity, key.uid],
                |row| row.get(0),
            )
            .optional()?;
        Ok(owned.unwrap_or(false))
    }

    /// Forgets a flag after we cleared it on the server. User-owned tombstones are kept.
    pub fn clear_flag_record(&self, key: &MessageKey) -> Result<()> {
        self.conn.execute(
            "DELETE FROM flag_ownership
             WHERE account = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4
               AND user_owned = 0",
            params![key.account, key.mailbox, key.uidvalidity, key.uid],
        )?;
        Ok(())
    }

    // ---- replies seen in Sent -------------------------------------------------------

    /// Remembers Message-IDs that the user's own sent mail answered. The ids are stored as
    /// given (the caller normalises them); nothing else about the sent mail is kept.
    pub fn add_reply_ids(&mut self, account: &str, ids: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO sent_replies (account, message_id) VALUES (?1, ?2)",
            )?;
            for id in ids {
                stmt.execute(params![account, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Whether a message with this (normalised) Message-ID has been answered from Sent.
    pub fn has_reply_to(&self, account: &str, message_id: &str) -> Result<bool> {
        let found = self
            .conn
            .query_row(
                "SELECT 1 FROM sent_replies WHERE account = ?1 AND message_id = ?2",
                params![account, message_id],
                |_| Ok(()),
            )
            .optional()?;
        Ok(found.is_some())
    }

    // ---- retry queue --------------------------------------------------------------

    /// Queues a message for another attempt, or bumps the attempt count if it is already
    /// queued. The caller chooses `next_attempt_at` (Unix seconds), so it owns the backoff.
    pub fn enqueue_retry(&self, key: &MessageKey, error: &str, next_attempt_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO retry_queue
               (account, mailbox, uidvalidity, uid, attempts, next_attempt_at, last_error)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)
             ON CONFLICT (account, mailbox, uidvalidity, uid) DO UPDATE SET
               attempts = retry_queue.attempts + 1,
               next_attempt_at = excluded.next_attempt_at,
               last_error = excluded.last_error",
            params![
                key.account,
                key.mailbox,
                key.uidvalidity,
                key.uid,
                next_attempt_at,
                truncate(error, MAX_REASON_CHARS),
            ],
        )?;
        Ok(())
    }

    /// Queued messages whose time has come, the longest-waiting first.
    pub fn due_retries(&self, account: &str, now: i64, limit: u32) -> Result<Vec<RetryItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, mailbox, uidvalidity, uid, attempts, next_attempt_at, last_error
             FROM retry_queue
             WHERE account = ?1 AND next_attempt_at <= ?2
             ORDER BY next_attempt_at, id
             LIMIT ?3",
        )?;
        let items = stmt
            .query_map(params![account, now, limit], |row| {
                Ok(RetryItem {
                    id: row.get(0)?,
                    key: MessageKey {
                        account: account.to_owned(),
                        mailbox: row.get(1)?,
                        uidvalidity: row.get(2)?,
                        uid: row.get(3)?,
                    },
                    attempts: row.get(4)?,
                    next_attempt_at: row.get(5)?,
                    last_error: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Removes a queue entry, after success or when giving up. Returns whether it existed.
    pub fn remove_retry(&self, id: i64) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM retry_queue WHERE id = ?1", params![id])?;
        Ok(removed > 0)
    }

    // ---- statistics (read-only) -----------------------------------------------------

    /// Messages per account and category, for rows logged at or after `since` (Unix
    /// seconds). Messages the model never judged (blocked sender, model failure) count
    /// under `"unclassified"`. Biggest counts first.
    pub fn category_counts(&self, since: i64) -> Result<Vec<AccountCount>> {
        self.account_counts(
            "SELECT account, COALESCE(category, 'unclassified'), COUNT(*) FROM processed
             WHERE created_at >= ?1
             GROUP BY 1, 2 ORDER BY 1, 3 DESC, 2",
            since,
        )
    }

    /// Messages per account and chosen flag since `since`. This counts the flag the engine
    /// chose, so in dry-run mode it shows the flags that *would* have been set.
    pub fn flag_counts(&self, since: i64) -> Result<Vec<AccountCount>> {
        self.account_counts(
            "SELECT account, flag, COUNT(*) FROM processed
             WHERE created_at >= ?1 AND flag IS NOT NULL
             GROUP BY 1, 2 ORDER BY 1, 3 DESC, 2",
            since,
        )
    }

    /// Shared by the two queries above: they differ only in SQL, not in row shape.
    fn account_counts(&self, sql: &str, since: i64) -> Result<Vec<AccountCount>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map(params![since], |row| {
                Ok(AccountCount {
                    account: row.get(0)?,
                    label: row.get(1)?,
                    count: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The `limit` senders with the most messages since `since`, in one account or (with
    /// `None`) across all of them. Ties are broken by address so the output is stable.
    pub fn top_senders(
        &self,
        since: i64,
        limit: u32,
        account: Option<&str>,
    ) -> Result<Vec<Counted>> {
        let mut stmt = self.conn.prepare(
            "SELECT sender, COUNT(*) AS n FROM processed
             WHERE created_at >= ?1 AND sender IS NOT NULL AND (?2 IS NULL OR account = ?2)
             GROUP BY sender ORDER BY n DESC, sender LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![since, account, limit], |row| {
                Ok(Counted {
                    label: row.get(0)?,
                    // SQLite integers are signed 64-bit; a COUNT is never negative.
                    count: row.get::<_, i64>(1)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The oldest and newest `processed` row since `since` (Unix seconds), or `None` when
    /// there are none.
    pub fn processed_range(&self, since: i64) -> Result<Option<(i64, i64)>> {
        // MIN and MAX over zero rows give one row of NULLs, hence the `Option`s.
        let (first, last): (Option<i64>, Option<i64>) = self.conn.query_row(
            "SELECT MIN(created_at), MAX(created_at) FROM processed WHERE created_at >= ?1",
            params![since],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(first.zip(last))
    }

    /// Per account: how many messages wait in the retry queue and how many are due at `now`.
    pub fn retry_queue_summary(&self, now: i64) -> Result<Vec<RetrySummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT account, COUNT(*), COALESCE(SUM(next_attempt_at <= ?1), 0)
             FROM retry_queue GROUP BY account ORDER BY account",
        )?;
        let rows = stmt
            .query_map(params![now], |row| {
                Ok(RetrySummary {
                    account: row.get(0)?,
                    queued: row.get::<_, i64>(1)? as u64,
                    due: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---- digest state -------------------------------------------------------------

    /// The date (`YYYY-MM-DD`) of the last digest written for an account.
    pub fn last_digest_date(&self, account: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_digest_date FROM digest_state WHERE account = ?1",
                params![account],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Saves the date (`YYYY-MM-DD`) of the digest just written, so a day is never repeated.
    pub fn set_last_digest_date(&self, account: &str, date: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO digest_state (account, last_digest_date) VALUES (?1, ?2)
             ON CONFLICT (account) DO UPDATE SET last_digest_date = excluded.last_digest_date",
            params![account, date],
        )?;
        Ok(())
    }

    // ---- labels -------------------------------------------------------------------

    /// Adds a row to the labelled dataset, replacing an earlier one for the same message.
    pub fn record_label(&self, label: &LabelRecord) -> Result<()> {
        let v = &label.verdict;
        self.conn.execute(
            "INSERT OR REPLACE INTO labels
               (account, mailbox, uidvalidity, uid, features, category, needs_reply,
                deadline, payment, confidence, flag, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                label.key.account,
                label.key.mailbox,
                label.key.uidvalidity,
                label.key.uid,
                label.features,
                category_to_str(v.category),
                v.needs_reply,
                v.deadline.map(|date| date.to_string()),
                payment_to_str(v.payment),
                v.confidence,
                label.flag.map(flag_to_str),
                truncate(&v.reason, MAX_REASON_CHARS),
                now_unix(),
            ],
        )?;
        Ok(())
    }

    /// Stores what the user decided the message really was, as free text such as
    /// `"category=newsletter"`. Returns whether a label existed.
    pub fn set_label_correction(&self, key: &MessageKey, correction: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE labels SET user_correction = ?5, corrected_at = ?6
             WHERE account = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
            params![
                key.account,
                key.mailbox,
                key.uidvalidity,
                key.uid,
                correction,
                now_unix()
            ],
        )?;
        Ok(changed > 0)
    }
}

// ---- migrations and file setup ----------------------------------------------------

/// Applies every migration the database has not seen, each in its own transaction.
fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let supported = MIGRATIONS.len() as i64;
    if current > supported {
        return Err(StoreError::SchemaTooNew {
            found: current,
            supported,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // `user_version` changes are transactional, so a failed script leaves no half step.
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // `recursive` creates missing parents too; existing directories are left untouched.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // The mode only applies when the file is created; an existing file keeps its own.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map(drop)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> std::io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map(drop)
}

// ---- small helpers ----------------------------------------------------------------

/// Current time as Unix seconds.
fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Cuts text to at most `max` characters (not bytes, so UTF-8 is never split).
fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn normalise_pattern(pattern: &str) -> Result<String> {
    let pattern = pattern.trim().to_lowercase();
    // "@" alone, or text with no "@" at all, cannot match any sender.
    if !pattern.contains('@') || pattern == "@" {
        return Err(StoreError::InvalidPattern(pattern));
    }
    Ok(pattern)
}

// ---- stable text values -----------------------------------------------------------
//
// The database stores enums as fixed text, not as Rust variant names or numbers, so
// renaming a variant in code can never silently change the meaning of stored data.

fn category_to_str(category: Category) -> &'static str {
    match category {
        Category::Personal => "personal",
        Category::Transactional => "transactional",
        Category::Newsletter => "newsletter",
        Category::Marketing => "marketing",
        Category::ColdOutreach => "cold_outreach",
        Category::Spam => "spam",
        Category::Phishing => "phishing",
    }
}

#[cfg(test)]
fn category_from_str(value: &str) -> Result<Category> {
    match value {
        "personal" => Ok(Category::Personal),
        "transactional" => Ok(Category::Transactional),
        "newsletter" => Ok(Category::Newsletter),
        "marketing" => Ok(Category::Marketing),
        "cold_outreach" => Ok(Category::ColdOutreach),
        "spam" => Ok(Category::Spam),
        "phishing" => Ok(Category::Phishing),
        other => unknown("category", other),
    }
}

fn payment_to_str(payment: Payment) -> &'static str {
    match payment {
        Payment::Unpaid => "unpaid",
        Payment::Paid => "paid",
        Payment::NotApplicable => "not_applicable",
    }
}

fn rule_to_str(rule: Rule) -> &'static str {
    match rule {
        Rule::Allow => "allow",
        Rule::Block => "block",
    }
}

fn rule_from_str(value: &str) -> Result<Rule> {
    match value {
        "allow" => Ok(Rule::Allow),
        "block" => Ok(Rule::Block),
        other => unknown("rule", other),
    }
}

fn flag_to_str(flag: TriageFlag) -> &'static str {
    match flag {
        TriageFlag::ReplyNeeded => "reply_needed",
        TriageFlag::Deadline => "deadline",
        TriageFlag::ToPay => "to_pay",
        TriageFlag::Bookkeeping => "bookkeeping",
        TriageFlag::Unsure => "unsure",
    }
}

fn flag_from_str(value: &str) -> Result<TriageFlag> {
    match value {
        "reply_needed" => Ok(TriageFlag::ReplyNeeded),
        "deadline" => Ok(TriageFlag::Deadline),
        "to_pay" => Ok(TriageFlag::ToPay),
        "bookkeeping" => Ok(TriageFlag::Bookkeeping),
        "unsure" => Ok(TriageFlag::Unsure),
        other => unknown("flag", other),
    }
}

fn colour_to_str(colour: FlagColour) -> &'static str {
    match colour {
        FlagColour::Red => "red",
        FlagColour::Orange => "orange",
        FlagColour::Yellow => "yellow",
        FlagColour::Green => "green",
        FlagColour::Blue => "blue",
        FlagColour::Purple => "purple",
        FlagColour::Gray => "gray",
    }
}

fn colour_from_str(value: &str) -> Result<FlagColour> {
    match value {
        "red" => Ok(FlagColour::Red),
        "orange" => Ok(FlagColour::Orange),
        "yellow" => Ok(FlagColour::Yellow),
        "green" => Ok(FlagColour::Green),
        "blue" => Ok(FlagColour::Blue),
        "purple" => Ok(FlagColour::Purple),
        "gray" => Ok(FlagColour::Gray),
        other => unknown("colour", other),
    }
}

fn unknown<T>(kind: &'static str, value: &str) -> Result<T> {
    Err(StoreError::UnknownValue {
        kind,
        value: value.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn key(uid: u32) -> MessageKey {
        MessageKey {
            account: "a".into(),
            mailbox: "INBOX".into(),
            uidvalidity: 1,
            uid,
        }
    }

    fn verdict() -> Verdict {
        Verdict {
            category: Category::Personal,
            needs_reply: true,
            deadline: Some(jiff::civil::date(2026, 11, 1)),
            payment: Payment::NotApplicable,
            confidence: 0.9,
            reason: "asks a question".into(),
        }
    }

    fn owned(uid: u32, set_at: i64) -> OwnedFlag {
        OwnedFlag {
            key: key(uid),
            message_id: Some(format!("<{uid}@x>")),
            colour: FlagColour::Orange,
            flag: Some(TriageFlag::ReplyNeeded),
            deadline: None,
            reason: Some("reply".into()),
            set_at,
        }
    }

    fn count(s: &Store, table: &str) -> i64 {
        s.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migrations_from_empty_create_all_tables() {
        let s = store();
        let version: i64 = s
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        for table in [
            "mailbox_state",
            "processed",
            "flag_ownership",
            "rules",
            "known_senders",
            "retry_queue",
            "digest_state",
            "labels",
        ] {
            assert_eq!(count(&s, table), 0, "{table} should exist and be empty");
        }
    }

    #[test]
    fn reopening_keeps_data_and_does_not_remigrate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        Store::open(&path)
            .unwrap()
            .add_known_sender("a", "x@y.com")
            .unwrap();
        let again = Store::open(&path).unwrap();
        assert!(again.is_known_sender("a", "x@y.com").unwrap());
    }

    #[test]
    fn newer_schema_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        assert!(matches!(
            migrate(&mut conn),
            Err(StoreError::SchemaTooNew { found: 99, .. })
        ));
    }

    #[test]
    fn rule_precedence_address_beats_domain() {
        let s = store();
        s.add_rule("a", "@Example.com", Rule::Block).unwrap();
        assert_eq!(
            s.match_rule("a", "Bob@EXAMPLE.com").unwrap(),
            Some(Rule::Block)
        );
        s.add_rule("a", "bob@example.com", Rule::Allow).unwrap();
        assert_eq!(
            s.match_rule("a", "bob@example.com").unwrap(),
            Some(Rule::Allow)
        );
        assert_eq!(
            s.match_rule("a", "eve@example.com").unwrap(),
            Some(Rule::Block)
        );
        assert_eq!(s.match_rule("a", "eve@other.com").unwrap(), None);
    }

    #[test]
    fn rule_precedence_account_beats_global_and_scopes_apply() {
        let s = store();
        s.add_rule(GLOBAL_ACCOUNT, "@x.com", Rule::Block).unwrap();
        assert_eq!(s.match_rule("a", "u@x.com").unwrap(), Some(Rule::Block));
        s.add_rule("a", "@x.com", Rule::Allow).unwrap();
        assert_eq!(s.match_rule("a", "u@x.com").unwrap(), Some(Rule::Allow));
        assert_eq!(s.match_rule("b", "u@x.com").unwrap(), Some(Rule::Block));
        // A global exact address still beats an account-level domain rule.
        s.add_rule(GLOBAL_ACCOUNT, "u@x.com", Rule::Block).unwrap();
        assert_eq!(s.match_rule("a", "u@x.com").unwrap(), Some(Rule::Block));
    }

    #[test]
    fn rules_can_be_changed_listed_and_removed() {
        let s = store();
        s.add_rule("*", "a@b.com", Rule::Allow).unwrap();
        s.add_rule("*", " A@B.com ", Rule::Block).unwrap();
        let rules = s.list_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule, Rule::Block);
        assert!(s.remove_rule("*", "a@b.com").unwrap());
        assert!(!s.remove_rule("*", "a@b.com").unwrap());
        assert!(matches!(
            s.add_rule("*", "nonsense", Rule::Allow),
            Err(StoreError::InvalidPattern(_))
        ));
    }

    #[test]
    fn known_senders_are_case_insensitive_and_counted_by_domain() {
        let s = store();
        assert!(s.add_known_sender("a", "Ann@Corp.com").unwrap());
        assert!(!s.add_known_sender("a", "ann@corp.com").unwrap());
        s.add_known_sender("a", "bob@corp.com").unwrap();
        s.add_known_sender("b", "cy@corp.com").unwrap();
        assert!(s.is_known_sender("a", "ANN@corp.com").unwrap());
        assert!(!s.is_known_sender("b", "ann@corp.com").unwrap());
        assert_eq!(s.known_domain_count("a", "Corp.com").unwrap(), 2);
        assert_eq!(s.known_domain_count("a", "@corp.com").unwrap(), 2);
        assert_eq!(s.known_domain_count("a", "none.org").unwrap(), 0);
    }

    #[test]
    fn mailbox_state_roundtrip_and_uidvalidity_reset() {
        let mut s = store();
        assert_eq!(s.mailbox_state("a", "INBOX").unwrap(), None);
        assert!(!s.set_mailbox_state("a", "INBOX", 1, 10).unwrap());
        assert!(!s.set_mailbox_state("a", "INBOX", 1, 20).unwrap());
        assert_eq!(
            s.mailbox_state("a", "INBOX").unwrap(),
            Some(MailboxState {
                uidvalidity: 1,
                last_uid: 20
            })
        );

        s.record_flag(&owned(5, 100)).unwrap();
        s.enqueue_retry(&key(6), "down", 50).unwrap();
        s.record_processed(&ProcessedRecord {
            key: key(5),
            message_id: None,
            sender: None,
            subject: None,
            verdict: None,
            flag: None,
            source: "model".into(),
        })
        .unwrap();

        assert!(s.set_mailbox_state("a", "INBOX", 2, 0).unwrap());
        assert_eq!(
            s.mailbox_state("a", "INBOX").unwrap(),
            Some(MailboxState {
                uidvalidity: 2,
                last_uid: 0
            })
        );
        // Old-numbering ownership and retries are gone; history stays.
        assert_eq!(count(&s, "flag_ownership"), 0);
        assert_eq!(count(&s, "retry_queue"), 0);
        assert_eq!(count(&s, "processed"), 1);
    }

    #[test]
    fn uidvalidity_reset_only_touches_its_own_mailbox() {
        let mut s = store();
        s.set_mailbox_state("a", "INBOX", 1, 1).unwrap();
        s.record_flag(&owned(5, 100)).unwrap();
        s.set_mailbox_state("a", "Other", 7, 1).unwrap();
        s.set_mailbox_state("a", "Other", 8, 1).unwrap();
        assert_eq!(s.owned_flags("a", "INBOX").unwrap().len(), 1);
    }

    #[test]
    fn processed_is_logged_without_body_and_truncated() {
        let s = store();
        let record = ProcessedRecord {
            key: key(1),
            message_id: Some("<1@x>".into()),
            sender: Some("bob@x.com".into()),
            subject: Some("é".repeat(1000)),
            verdict: Some(verdict()),
            flag: Some(TriageFlag::ReplyNeeded),
            source: "model".into(),
        };
        assert!(!s.is_processed(&key(1)).unwrap());
        s.record_processed(&record).unwrap();
        s.record_processed(&record).unwrap(); // replaces, no duplicate
        assert!(s.is_processed(&key(1)).unwrap());
        assert_eq!(count(&s, "processed"), 1);
        let (subject, category, flag): (String, String, String) = s
            .conn
            .query_row("SELECT subject, category, flag FROM processed", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(subject.chars().count(), MAX_SUBJECT_CHARS);
        assert_eq!(category_from_str(&category).unwrap(), Category::Personal);
        assert_eq!(flag, "reply_needed");
        let columns: Vec<String> = s
            .conn
            .prepare("SELECT name FROM pragma_table_info('processed')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(!columns.iter().any(|c| c.contains("body")));
    }

    #[test]
    fn ownership_lifecycle_user_change_is_final() {
        let s = store();
        s.record_flag(&owned(1, 100)).unwrap();
        s.record_flag(&owned(2, 200)).unwrap();
        assert_eq!(s.owned_flags("a", "INBOX").unwrap().len(), 2);

        // The user changed message 1's flag.
        assert!(s.mark_user_owned(&key(1)).unwrap());
        assert!(s.is_user_owned(&key(1)).unwrap());
        let owned_now = s.owned_flags("a", "INBOX").unwrap();
        assert_eq!(owned_now.len(), 1);
        assert_eq!(owned_now[0].key.uid, 2);

        // Neither re-recording nor clearing can take it back.
        s.record_flag(&owned(1, 300)).unwrap();
        s.clear_flag_record(&key(1)).unwrap();
        assert!(s.is_user_owned(&key(1)).unwrap());
        assert_eq!(s.owned_flags("a", "INBOX").unwrap().len(), 1);

        // A flag we own is forgotten after we clear it.
        s.clear_flag_record(&key(2)).unwrap();
        assert!(s.owned_flags("a", "INBOX").unwrap().is_empty());
        assert!(!s.mark_user_owned(&key(99)).unwrap());
    }

    #[test]
    fn the_flag_kind_and_deadline_survive_a_round_trip() {
        let s = store();
        let mut flag = owned(1, 100);
        flag.colour = FlagColour::Yellow;
        flag.flag = Some(TriageFlag::Deadline);
        flag.deadline = Some(jiff::civil::date(2026, 11, 30));
        s.record_flag(&flag).unwrap();
        let back = s.owned_flags("a", "INBOX").unwrap();
        assert_eq!(back, [flag]);
    }

    #[test]
    fn replies_seen_in_sent_are_remembered_per_account() {
        let mut s = store();
        s.add_reply_ids("a", &["one@x".into(), "two@x".into()])
            .unwrap();
        // Adding the same id again is harmless.
        s.add_reply_ids("a", &["one@x".into()]).unwrap();
        assert!(s.has_reply_to("a", "one@x").unwrap());
        assert!(s.has_reply_to("a", "two@x").unwrap());
        assert!(!s.has_reply_to("a", "three@x").unwrap());
        assert!(!s.has_reply_to("other", "one@x").unwrap());
    }

    #[test]
    fn recording_a_flag_again_updates_the_colour() {
        let s = store();
        s.record_flag(&owned(1, 100)).unwrap();
        let mut again = owned(1, 200);
        again.colour = FlagColour::Gray;
        s.record_flag(&again).unwrap();
        let flags = s.owned_flags("a", "INBOX").unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].colour, FlagColour::Gray);
        assert_eq!(flags[0].set_at, 200);
    }

    #[test]
    fn retry_queue_orders_by_due_time_and_counts_attempts() {
        let s = store();
        s.enqueue_retry(&key(1), "down", 300).unwrap();
        s.enqueue_retry(&key(2), "down", 100).unwrap();
        s.enqueue_retry(&key(3), "down", 200).unwrap();
        s.enqueue_retry(&key(4), "down", 900).unwrap();

        let due = s.due_retries("a", 500, 10).unwrap();
        let uids: Vec<u32> = due.iter().map(|i| i.key.uid).collect();
        assert_eq!(uids, [2, 3, 1]); // 4 is not due yet
        assert_eq!(s.due_retries("a", 500, 2).unwrap().len(), 2);
        assert!(s.due_retries("other", 500, 10).unwrap().is_empty());

        // A second failure bumps the count and pushes it back.
        s.enqueue_retry(&key(2), "still down", 1000).unwrap();
        assert_eq!(s.due_retries("a", 500, 10).unwrap().len(), 2);
        let all = s.due_retries("a", 2000, 10).unwrap();
        let two = all.iter().find(|i| i.key.uid == 2).unwrap();
        assert_eq!(two.attempts, 2);
        assert_eq!(two.last_error.as_deref(), Some("still down"));

        assert!(s.remove_retry(two.id).unwrap());
        assert!(!s.remove_retry(two.id).unwrap());
        assert_eq!(s.due_retries("a", 2000, 10).unwrap().len(), 3);
    }

    #[test]
    fn digest_date_roundtrip() {
        let s = store();
        assert_eq!(s.last_digest_date("a").unwrap(), None);
        s.set_last_digest_date("a", "2026-10-25").unwrap();
        s.set_last_digest_date("a", "2026-10-26").unwrap();
        assert_eq!(
            s.last_digest_date("a").unwrap().as_deref(),
            Some("2026-10-26")
        );
        assert_eq!(s.last_digest_date("b").unwrap(), None);
    }

    #[test]
    fn labels_record_and_accept_a_correction() {
        let s = store();
        let label = LabelRecord {
            key: key(1),
            features: r#"{"list_unsubscribe":true}"#.into(),
            verdict: verdict(),
            flag: None,
        };
        s.record_label(&label).unwrap();
        s.record_label(&label).unwrap();
        assert_eq!(count(&s, "labels"), 1);
        assert!(
            s.set_label_correction(&key(1), "category=newsletter")
                .unwrap()
        );
        assert!(!s.set_label_correction(&key(2), "x").unwrap());
        let correction: String = s
            .conn
            .query_row("SELECT user_correction FROM labels", [], |r| r.get(0))
            .unwrap();
        assert_eq!(correction, "category=newsletter");
    }

    #[test]
    fn enum_text_values_roundtrip() {
        for c in [
            Category::Personal,
            Category::Transactional,
            Category::Newsletter,
            Category::Marketing,
            Category::ColdOutreach,
            Category::Spam,
            Category::Phishing,
        ] {
            assert_eq!(category_from_str(category_to_str(c)).unwrap(), c);
        }
        for c in [
            FlagColour::Red,
            FlagColour::Orange,
            FlagColour::Yellow,
            FlagColour::Green,
            FlagColour::Blue,
            FlagColour::Purple,
            FlagColour::Gray,
        ] {
            assert_eq!(colour_from_str(colour_to_str(c)).unwrap(), c);
        }
        assert!(matches!(
            colour_from_str("pink"),
            Err(StoreError::UnknownValue { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn new_database_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let path = data_dir.join("state.db");
        let s = Store::open(&path).unwrap();
        s.add_known_sender("a", "x@y.com").unwrap(); // makes the -wal file appear
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&data_dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        let wal = data_dir.join("state.db-wal");
        if wal.exists() {
            assert_eq!(mode(&wal), 0o600);
        }
    }

    // ---- statistics ---

    /// Inserts a `processed` row directly, so a test can choose the timestamp.
    fn log(
        s: &Store,
        uid: u32,
        account: &str,
        sender: &str,
        labels: (Option<&str>, Option<&str>),
        at: i64,
    ) {
        let (category, flag) = labels;
        s.conn
            .execute(
                "INSERT INTO processed
                   (account, mailbox, uidvalidity, uid, sender, category, flag, source, created_at)
                 VALUES (?1, 'INBOX', 1, ?2, ?3, ?4, ?5, 'model', ?6)",
                params![account, uid, sender, category, flag, at],
            )
            .unwrap();
    }

    fn ac(account: &str, label: &str, count: u64) -> AccountCount {
        AccountCount {
            account: account.into(),
            label: label.into(),
            count,
        }
    }

    fn counted(label: &str, count: u64) -> Counted {
        Counted {
            label: label.into(),
            count,
        }
    }

    #[test]
    fn category_and_flag_counts_group_by_account_and_respect_since() {
        let s = store();
        log(&s, 1, "a", "x@x.com", (Some("newsletter"), None), 100);
        log(&s, 2, "a", "x@x.com", (Some("newsletter"), None), 200);
        log(
            &s,
            3,
            "a",
            "y@y.com",
            (Some("personal"), Some("reply_needed")),
            300,
        );
        log(&s, 4, "a", "z@z.com", (None, None), 400);
        log(
            &s,
            5,
            "b",
            "x@x.com",
            (Some("personal"), Some("reply_needed")),
            500,
        );
        log(&s, 6, "b", "old@x.com", (Some("spam"), None), 10);

        assert_eq!(
            s.category_counts(0).unwrap(),
            vec![
                ac("a", "newsletter", 2),
                ac("a", "personal", 1),
                ac("a", "unclassified", 1),
                ac("b", "personal", 1),
                ac("b", "spam", 1),
            ]
        );
        // The cut-off hides the old spam row.
        let recent = s.category_counts(50).unwrap();
        assert!(!recent.iter().any(|c| c.label == "spam"));
        assert_eq!(
            s.flag_counts(0).unwrap(),
            vec![ac("a", "reply_needed", 1), ac("b", "reply_needed", 1)]
        );
    }

    #[test]
    fn top_senders_can_be_limited_to_an_account() {
        let s = store();
        log(&s, 1, "a", "x@x.com", (None, None), 100);
        log(&s, 2, "a", "x@x.com", (None, None), 100);
        log(&s, 3, "a", "y@y.com", (None, None), 100);
        log(&s, 4, "b", "y@y.com", (None, None), 100);
        log(&s, 5, "b", "y@y.com", (None, None), 100);
        log(&s, 6, "b", "y@y.com", (None, None), 100);

        assert_eq!(
            s.top_senders(0, 10, None).unwrap(),
            vec![counted("y@y.com", 4), counted("x@x.com", 2)]
        );
        assert_eq!(
            s.top_senders(0, 10, Some("a")).unwrap(),
            vec![counted("x@x.com", 2), counted("y@y.com", 1)]
        );
        assert_eq!(s.top_senders(0, 1, None).unwrap().len(), 1);
    }

    #[test]
    fn processed_range_is_none_when_empty() {
        let s = store();
        assert_eq!(s.processed_range(0).unwrap(), None);
        log(&s, 1, "a", "x@x.com", (None, None), 100);
        log(&s, 2, "a", "x@x.com", (None, None), 300);
        assert_eq!(s.processed_range(0).unwrap(), Some((100, 300)));
        assert_eq!(s.processed_range(200).unwrap(), Some((300, 300)));
    }

    #[test]
    fn retry_queue_summary_counts_queued_and_due() {
        let s = store();
        assert!(s.retry_queue_summary(500).unwrap().is_empty());
        s.enqueue_retry(&key(1), "down", 100).unwrap();
        s.enqueue_retry(&key(2), "down", 900).unwrap();
        assert_eq!(
            s.retry_queue_summary(500).unwrap(),
            vec![RetrySummary {
                account: "a".into(),
                queued: 2,
                due: 1
            }]
        );
    }
}
