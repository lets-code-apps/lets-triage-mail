//! The `status` command: one snapshot of what the engine has done today, for machines.
//!
//! # Shape
//!
//! Like [`check`](crate::check) and `stats`, every decision lives in pure functions here;
//! the command wrapper (`cli/status.rs`) only opens the database and prints. The JSON shape is
//! **locked by a test** (`json_shape_is_locked`): renaming a key or value is a breaking change
//! for the desktop app, which parses stdout with a defensive reader (`desktop/src/status-report.ts`).
//!
//! # Where the data comes from
//!
//! A one-shot process cannot see the running engine's memory, so `run` persists what `status`
//! needs into SQLite (migration 5: `engine_status` + `account_status`) and refreshes a heartbeat
//! every 30 s. `status` answers even with the engine stopped: counts come from the `processed`
//! history, and every account then reads `not_running`. See `docs/DECISIONS.md` (2026-11-07)
//! for why this is the database and not a socket.
//!
//! # Hard rules
//!
//! Metadata only, like every other store read: the report carries names from the *config*, coarse
//! states and three numbers — never senders, subjects or bodies, and never raw server error text.
//! Each state's `reason` comes from a fixed English table.

use jiff::Timestamp;
use serde::Serialize;

use crate::config::{Config, FlagMode};
use crate::digest::build::would_be_flag;
use crate::engine::{Category, FlagPolicy};
use crate::store::DigestRow;

/// Shape version. The desktop refuses to guess: changing any key or value is breaking for it.
pub const STATUS_VERSION: u8 = 1;

/// `running` is only reported while the heartbeat is this fresh. `run` writes one every 30 s,
/// so three beats of slack covers a busy blocking call; a clean stop deletes the row and reports
/// "not running" at once.
pub const HEARTBEAT_STALE_SECONDS: i64 = 90;

/// The only window kind v1 reports; a future `--since` variant would add others.
pub const WINDOW_KIND: &str = "today";

// ---------------------------------------------------------------------------------------------
// Account state
// ---------------------------------------------------------------------------------------------

/// How far along one account's worker is, as `run` records it. Stored as the snake_case text
/// of the variant; the enum, not a free string, is what keeps the desktop's state pills honest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    /// The engine started this account but it has not logged in yet.
    Connecting,
    /// Logged in and watching the inbox.
    Connected,
    /// A connection came and went; the worker is retrying with backoff.
    Reconnecting,
    /// The server refused the login; the supervisor restarts the worker slowly.
    SignInFailed,
    /// The worker stopped in a way restarting cannot fix.
    Failed,
    /// No engine is running (a stored state would be stale, so it is not used).
    NotRunning,
}

impl AccountState {
    /// The DB text for this state (`account_status.state`).
    pub fn as_str(self) -> &'static str {
        match self {
            AccountState::Connecting => "connecting",
            AccountState::Connected => "connected",
            AccountState::Reconnecting => "reconnecting",
            AccountState::SignInFailed => "sign_in_failed",
            AccountState::Failed => "failed",
            AccountState::NotRunning => "not_running",
        }
    }

    /// The note shown next to the state: one fixed English sentence per state, chosen here,
    /// never text that came from a server or an exception (raw error text can carry host and
    /// user names; the desktop renders this verbatim as `textContent`).
    pub fn reason(self) -> &'static str {
        match self {
            AccountState::Connecting => "Connecting.",
            AccountState::Connected => "Watching the inbox.",
            AccountState::Reconnecting => "Connection lost. Retrying.",
            AccountState::SignInFailed => "Sign-in failed. Check the password.",
            AccountState::Failed => "Stopped after an error.",
            AccountState::NotRunning => "Engine not running.",
        }
    }
}

/// Reads back a stored state leniently: a value this program never writes (a newer engine,
/// a hand edit) becomes `failed` — showing a problem beats crashing or pretending. Named like
/// the store's other `*_from_str` helpers so it cannot be confused with `FromStr::from_str`.
pub fn state_from_str(value: &str) -> AccountState {
    match value {
        "connecting" => AccountState::Connecting,
        "connected" => AccountState::Connected,
        "reconnecting" => AccountState::Reconnecting,
        "sign_in_failed" => AccountState::SignInFailed,
        "failed" => AccountState::Failed,
        // Unknown, including "not_running" (that one only means "engine down", which
        // `build_report` decides for itself).
        other => {
            tracing::warn!(
                state = %other,
                "unknown account state in the database; reading it as failed"
            );
            AccountState::Failed
        }
    }
}

/// Whether the engine is alive: its heartbeat row exists and is fresh. Exactly at the limit
/// still counts (a test pins the boundary at 90 s).
pub fn engine_is_running(heartbeat_at: Option<i64>, now: i64) -> bool {
    heartbeat_at.is_some_and(|at| now - at <= HEARTBEAT_STALE_SECONDS)
}

/// "Today" for the counts: local midnight of `now` in `tz` — the same clock the daily digest
/// uses for "today". The zone is a parameter so tests use a fixed offset
/// (`jiff::tz::offset(2).to_time_zone()`) instead of the machine's zone. A day start that
/// cannot be represented (timestamps near jiff's limits) falls back to `now`, which asks for
/// an empty window — the safe direction.
pub fn start_of_day(now: Timestamp, tz: jiff::tz::TimeZone) -> i64 {
    let day_start: Timestamp = match now.to_zoned(tz.clone()).start_of_day() {
        Ok(zoned) => zoned.timestamp(),
        Err(_) => now,
    };
    day_start.as_second()
}

// ---------------------------------------------------------------------------------------------
// Count buckets
// ---------------------------------------------------------------------------------------------

/// Which of today's three buckets one processed message falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    /// Flagged, or (on a classify-only account) would be flagged: the digest's "needs you".
    ToLookAt,
    /// Neither: unflagged personal/transactional mail, or the model never judged it.
    Fyi,
    /// Obvious noise the user has already got: newsletter, marketing, cold outreach, spam
    /// or phishing, and not flagged.
    Done,
}

/// The bucket of one row. Same definition as the digest (see `digest/build.rs`), on purpose:
/// the two views must never disagree about "needs you" or "noise".
pub fn bucket(row: &DigestRow, derive_flags: Option<&FlagPolicy>) -> Bucket {
    // A stored flag, or on a classify-only account the would-be flag from the verdict
    // (`Trust::Unknown` exactly like the digest; `would_be_flag` returns `None` when
    // `derive_flags` is `None`, so stored-flag accounts take the first branch).
    let needs_the_user = row.flag.is_some() || would_be_flag(row, derive_flags).is_some();
    if needs_the_user {
        return Bucket::ToLookAt;
    }
    match row.verdict.as_ref().map(|verdict| verdict.category) {
        Some(
            Category::Newsletter
            | Category::Marketing
            | Category::ColdOutreach
            | Category::Spam
            | Category::Phishing,
        ) => Bucket::Done,
        Some(Category::Personal | Category::Transactional) | None => Bucket::Fyi,
    }
}

/// Three counters for one account, or for all of them together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub to_look_at: u64,
    pub fyi: u64,
    pub done: u64,
}

impl Counts {
    pub fn zero() -> Counts {
        Counts {
            to_look_at: 0,
            fyi: 0,
            done: 0,
        }
    }

    /// Elementwise sum (the totals line).
    pub fn add(&self, other: &Counts) -> Counts {
        Counts {
            to_look_at: self.to_look_at + other.to_look_at,
            fyi: self.fyi + other.fyi,
            done: self.done + other.done,
        }
    }

    /// The three numbers for one account's rows of today.
    pub fn for_rows(rows: &[DigestRow], derive_flags: Option<&FlagPolicy>) -> Counts {
        let mut counts = Counts::zero();
        for row in rows {
            match bucket(row, derive_flags) {
                Bucket::ToLookAt => counts.to_look_at += 1,
                Bucket::Fyi => counts.fyi += 1,
                Bucket::Done => counts.done += 1,
            }
        }
        counts
    }
}

// ---------------------------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------------------------

/// The `engine` object: liveness plus, while running, when the run started and its last
/// heartbeat. Both times are `null` when `running` is false — an `Option` serialises to `null`
/// (no `skip_serializing_if`), which is what keeps the locked shape's keys always present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EngineInfo {
    pub running: bool,
    pub started_at: Option<i64>,
    pub heartbeat_at: Option<i64>,
}

/// The `window` object: which period the counts cover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Window {
    pub kind: String,
    pub since: i64,
}

/// One entry of the `accounts` array: config name and mode, state and reason, own counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccountEntry {
    pub name: String,
    pub mode: String,
    pub state: AccountState,
    pub reason: String,
    pub counts: Counts,
}

/// Everything `status --json` prints. Field order is JSON key order; `generated_at` is passed
/// in so tests are deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub version: u8,
    pub generated_at: i64,
    pub engine: EngineInfo,
    pub window: Window,
    pub counts: Counts,
    pub accounts: Vec<AccountEntry>,
}

/// The raw rows the store query returned, before this module shapes them. Mirrors how `stats`
/// separates `query` from `build_report`.
pub struct RawStatus {
    /// The `engine_status` row (`started_at`, `heartbeat_at`); `None` when no run is recorded.
    pub engine: Option<(i64, i64)>,
    /// The `account_status` rows, as the engine wrote them.
    pub states: Vec<(String, String)>,
    /// Today's `processed` rows per account, in config order.
    pub rows: Vec<Vec<DigestRow>>,
}

/// Pure: the config and the raw store rows in, the exact report out. `since` is `start_of_day`
/// at the call site; the window filter itself happened in the SQL read.
pub fn build_report(now: i64, since: i64, config: &Config, raw: RawStatus) -> StatusReport {
    // Liveness from the heartbeat; the times are reported only while running.
    let running = engine_is_running(raw.engine.map(|(_, heartbeat)| heartbeat), now);
    let (started_at, heartbeat_at) = raw.engine.unwrap_or((0, 0));
    let policy = config.flag_policy();

    let accounts: Vec<AccountEntry> = config
        .accounts
        .iter()
        .zip(raw.rows)
        .map(|(account, rows)| {
            // Engine down: every stored state is stale, so every account is `not_running`.
            // Engine up: no row yet means the worker has not reached the server.
            let state: AccountState = if !running {
                AccountState::NotRunning
            } else {
                match raw.states.iter().find(|(name, _)| *name == account.name) {
                    None => AccountState::Connecting,
                    Some((_, text)) => state_from_str(text),
                }
            };
            let mode = account.flag_mode(&config.policy);
            // A classify-only account stores no flags; derive would-be flags like the digest.
            let derive_flags = (mode == FlagMode::ClassifyOnly).then_some(&policy);
            AccountEntry {
                name: account.name.to_owned(),
                mode: mode.as_str().to_owned(),
                state,
                reason: state.reason().to_owned(),
                counts: Counts::for_rows(&rows, derive_flags),
            }
        })
        .collect();

    // `zip` pairs config accounts with their rows; counts of the same order.
    let totals = accounts
        .iter()
        .map(|account| account.counts.clone())
        .fold(Counts::zero(), |sum, counts| sum.add(&counts));

    StatusReport {
        version: STATUS_VERSION,
        generated_at: now,
        engine: EngineInfo {
            running,
            started_at: if running { Some(started_at) } else { None },
            heartbeat_at: if running { Some(heartbeat_at) } else { None },
        },
        window: Window {
            kind: WINDOW_KIND.to_owned(),
            since,
        },
        counts: totals,
        accounts,
    }
}

/// The same report as short human text, so `status` is useful from a terminal too.
pub fn format_text(report: &StatusReport) -> String {
    let mut out = String::new();
    if report.engine.running {
        out.push_str("Engine: running\n");
    } else {
        out.push_str("Engine: not running\n");
    }
    for account in report.accounts.iter() {
        out.push_str(&format!(
            "  {} ({}): {} — {}\n",
            account.name,
            account.mode,
            account.state.as_str(),
            account.reason,
        ));
    }
    out.push_str(&format!(
        "Today so far: {} to look at, {} fyi, {} done\n",
        report.counts.to_look_at, report.counts.fyi, report.counts.done,
    ));
    out
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    use crate::engine::{Payment, TriageFlag, Verdict};

    fn row(
        sender: &str,
        subject: &str,
        flag: Option<TriageFlag>,
        verdict: Option<Verdict>,
        at: i64,
    ) -> DigestRow {
        DigestRow {
            sender: Some(sender.to_owned()),
            subject: Some(subject.to_owned()),
            flag,
            verdict,
            created_at: at,
        }
    }

    fn verdict(category: Category) -> Verdict {
        Verdict {
            category,
            needs_reply: false,
            deadline: None,
            payment: Payment::NotApplicable,
            confidence: 0.9,
            reason: "test verdict".into(),
        }
    }

    // Two accounts in config order: "Example Work" (live with `first_extra`) and
    // "Example Home" (classify-only).
    fn config_two(first_extra: &str) -> Config {
        Config::parse(&format!(
            "[[accounts]]\nname = \"Example Work\"\nhost = \"h\"\nusername = \"u\"\n{first_extra}\n[[accounts]]\nname = \"Example Home\"\nhost = \"h\"\nusername = \"u\"\nflagging = false\n"
        ))
            .unwrap()
    }

    // --- the locked JSON shape ---

    #[test]
    fn json_shape_is_locked() {
        // The exact shape from docs/DESKTOP.md: every key present, `null` never a missing key.
        let report = StatusReport {
            version: 1,
            generated_at: 1794038100,
            engine: EngineInfo {
                running: true,
                started_at: Some(1794000000),
                heartbeat_at: Some(1794038090),
            },
            window: Window {
                kind: "today".to_owned(),
                since: 1793998800,
            },
            counts: Counts {
                to_look_at: 3,
                fyi: 14,
                done: 22,
            },
            accounts: [
                AccountEntry {
                    name: "Example Work".into(),
                    mode: "live".into(),
                    state: AccountState::Connected,
                    reason: "Watching the inbox.".into(),
                    counts: Counts {
                        to_look_at: 3,
                        fyi: 14,
                        done: 22,
                    },
                },
                AccountEntry {
                    name: "Example Home".into(),
                    mode: "classify-only".into(),
                    state: AccountState::Reconnecting,
                    reason: "Connection lost. Retrying.".into(),
                    counts: Counts {
                        to_look_at: 0,
                        fyi: 0,
                        done: 0,
                    },
                },
            ]
            .to_vec(),
        };
        let value: Value = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(
            value,
            json!({
                "version": 1,
                "generated_at": 1794038100,
                "engine": { "running": true, "started_at": 1794000000, "heartbeat_at": 1794038090 },
                "window": { "kind": "today", "since": 1793998800 },
                "counts": { "to_look_at": 3, "fyi": 14, "done": 22 },
                "accounts": [
                    {
                        "name": "Example Work",
                        "mode": "live",
                        "state": "connected",
                        "reason": "Watching the inbox.",
                        "counts": { "to_look_at": 3, "fyi": 14, "done": 22 }
                    },
                    {
                        "name": "Example Home",
                        "mode": "classify-only",
                        "state": "reconnecting",
                        "reason": "Connection lost. Retrying.",
                        "counts": { "to_look_at": 0, "fyi": 0, "done": 0 }
                    }
                ]
            })
        );
    }

    #[test]
    fn not_running_shape_keeps_the_keys_with_nulls() {
        let value: Value = serde_json::from_str(
            &serde_json::to_string(&EngineInfo {
                running: false,
                started_at: None,
                heartbeat_at: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({ "running": false, "started_at": null, "heartbeat_at": null })
        );
    }

    #[test]
    fn states_serialize_as_snake_case() {
        let pairs: [(&AccountState, &str); 6] = [
            (&AccountState::Connecting, "connecting"),
            (&AccountState::Connected, "connected"),
            (&AccountState::Reconnecting, "reconnecting"),
            (&AccountState::SignInFailed, "sign_in_failed"),
            (&AccountState::Failed, "failed"),
            (&AccountState::NotRunning, "not_running"),
        ];
        for (state, text) in pairs {
            assert_eq!(state.as_str(), text);
            assert_eq!(json!({ "s": state }), json!({ "s": text }), "{text}");
            // And the DB round-trip, except `not_running`: that text is never stored (an
            // engine-down report is decided by `build_report`), so it reads back as `failed`.
            if *state != AccountState::NotRunning {
                assert_eq!(state_from_str(text), *state);
            }
        }
        // Lenient for unknown text: a newer engine must not break an older `status`.
        assert_eq!(state_from_str("quacking"), AccountState::Failed);
        assert_eq!(state_from_str("not_running"), AccountState::Failed);
    }

    #[test]
    fn reasons_are_a_fixed_table_without_raw_error_text() {
        let table: [(&AccountState, &str); 6] = [
            (&AccountState::Connecting, "Connecting."),
            (&AccountState::Connected, "Watching the inbox."),
            (&AccountState::Reconnecting, "Connection lost. Retrying."),
            (
                &AccountState::SignInFailed,
                "Sign-in failed. Check the password.",
            ),
            (&AccountState::Failed, "Stopped after an error."),
            (&AccountState::NotRunning, "Engine not running."),
        ];
        for (state, expected) in table {
            assert_eq!(state.reason(), expected);
        }
        // No state's note can carry host names or exception text: the table is closed.
        let notes = table.iter().map(|(_, note)| note);
        for note in notes {
            for forbidden in [":", " at ", "imap", "TLS", "@"] {
                assert!(!note.contains(forbidden), "{note}");
            }
        }
    }

    // --- liveness ---

    #[test]
    fn engine_is_running_only_while_heartbeat_is_fresh() {
        assert!(!engine_is_running(None, 1000)); // no row: not running
        assert!(engine_is_running(Some(1000), 1000)); // fresh
        // The boundary: exactly HEARTBEAT_STALE_SECONDS is still running.
        assert!(engine_is_running(
            Some(1000 - HEARTBEAT_STALE_SECONDS),
            1000
        ));
        assert!(!engine_is_running(
            Some(1000 - HEARTBEAT_STALE_SECONDS - 1),
            1000
        ));
    }
    // --- what `status` answers when the engine is down ---

    #[test]
    fn not_running_overrides_stored_account_states() {
        let config = config_two("");
        // A stored row says "connected", but the heartbeat is old (a hard-killed engine).
        let raw = RawStatus {
            engine: Some((100, 100)),
            states: [("Example Work".to_string(), "connected".to_string())].to_vec(),
            rows: [Vec::<DigestRow>::new(), Vec::<DigestRow>::new()].to_vec(),
        };
        let report = build_report(1000, 900, &config, raw);
        assert!(!report.engine.running);
        assert_eq!(report.engine.started_at, None);
        assert_eq!(
            report.accounts.iter().map(|a| a.state).collect::<Vec<_>>(),
            [AccountState::NotRunning, AccountState::NotRunning].to_vec(),
        );
    }

    #[test]
    fn empty_database_still_answers() {
        // A fresh install: no engine row, no states, no rows — a valid, honest report.
        let config = config_two("");
        let raw = RawStatus {
            engine: None,
            states: Vec::<(String, String)>::new(),
            rows: [Vec::<DigestRow>::new(), Vec::<DigestRow>::new()].to_vec(),
        };
        let report = build_report(1000, 900, &config, raw);
        assert_eq!(
            report.engine,
            EngineInfo {
                running: false,
                started_at: None,
                heartbeat_at: None,
            },
        );
        assert_eq!(report.counts, Counts::zero());
        let value = serde_json::to_string(&report).unwrap();
        assert!(value.contains("\"accounts\""));
        assert!(value.contains("not_running"));
    }

    #[test]
    fn engine_up_and_no_state_row_yet_is_connecting() {
        let config = config_two("");
        let raw = RawStatus {
            engine: Some((940, 1000)), // heartbeat fresh at now=1000
            states: Vec::<(String, String)>::new(),
            rows: [Vec::<DigestRow>::new(), Vec::<DigestRow>::new()].to_vec(),
        };
        let report = build_report(1000, 900, &config, raw);
        assert!(report.engine.running);
        assert_eq!(report.engine.started_at, Some(940));
        assert_eq!(
            report.accounts.iter().map(|a| a.state).collect::<Vec<_>>(),
            [AccountState::Connecting, AccountState::Connecting].to_vec(),
        );
    }

    // --- counts window ---

    #[test]
    fn counts_window_starts_at_local_midnight() {
        // A fixed +02:00 zone, not the machine's: local midnight is 22:00 UTC.
        let tz = jiff::tz::offset(2).to_time_zone();
        let midnight_seconds = tz
            .to_timestamp(jiff::civil::datetime(2026, 11, 7, 0, 0, 0, 0))
            .unwrap()
            .as_second();
        // One second after local midnight is still "today"; one second before belongs to
        // the previous day.
        let just_after = Timestamp::from_second(midnight_seconds + 1).unwrap();
        assert_eq!(start_of_day(just_after, tz.clone()), midnight_seconds);
        let just_before = Timestamp::from_second(midnight_seconds - 1).unwrap();
        assert_eq!(start_of_day(just_before, tz), midnight_seconds - 24 * 3600);
    }

    // --- buckets ---

    #[test]
    fn buckets_cover_every_row_exactly_once() {
        let rows = [
            row(
                "a@x.com",
                "Reply",
                Some(TriageFlag::ReplyNeeded),
                Some(verdict(Category::Personal)),
                1,
            ),
            row(
                "b@x.com",
                "Newsletter",
                None,
                Some(verdict(Category::Newsletter)),
                2,
            ),
            row(
                "c@x.com",
                "Marketing",
                None,
                Some(verdict(Category::Marketing)),
                3,
            ),
            row(
                "d@x.com",
                "Cold",
                None,
                Some(verdict(Category::ColdOutreach)),
                4,
            ),
            row("e@x.com", "Spam", None, Some(verdict(Category::Spam)), 5),
            row(
                "f@x.com",
                "Phishing",
                None,
                Some(verdict(Category::Phishing)),
                6,
            ),
            row(
                "g@x.com",
                "Quiet personal",
                None,
                Some(verdict(Category::Personal)),
                7,
            ),
            row("h@x.com", "No verdict", None, None, 8),
            row(
                "i@x.com",
                "Flagged newsletter",
                Some(TriageFlag::ToPay),
                Some(verdict(Category::Newsletter)),
                9,
            ),
        ];
        let counts = Counts::for_rows(&rows, None);
        // Flagged rows count "to look at" even when their category is noise; a flagged
        // newsletter is not `done`.
        assert_eq!(
            counts,
            Counts {
                to_look_at: 2,
                fyi: 2,
                done: 5,
            },
        );
        assert_eq!(
            counts.to_look_at + counts.fyi + counts.done,
            rows.len() as u64
        );
    }

    #[test]
    fn classify_only_counts_would_be_flags() {
        // A confident "reply needed" personal verdict with no stored flag (the account never
        // writes flags): the digest counts it as "needs you", so status must too.
        let asking = Verdict {
            category: Category::Personal,
            needs_reply: true,
            deadline: None,
            payment: Payment::NotApplicable,
            confidence: 0.9,
            reason: "asks a question".into(),
        };
        let rows = [row("p@x.com", "Call me", None, Some(asking), 1)];
        let policy = config_two("").flag_policy();
        // Stored flags (a live or dry-run account): nothing was flagged.
        assert_eq!(Counts::for_rows(&rows, None).to_look_at, 0);
        // Derive (classify-only): it would be flagged.
        assert_eq!(Counts::for_rows(&rows, Some(&policy)).to_look_at, 1);
    }

    #[test]
    fn to_look_at_equals_the_digest_needs_you_count() {
        // The same rows through `digest::build` and through the status buckets must agree
        // for both derive modes; this is what stops the two views ever disagreeing.
        use crate::digest::build::{DigestInput, build};

        let rows = [
            row("a@x.com", "Reply", Some(TriageFlag::ReplyNeeded), None, 1),
            row(
                "b@x.com",
                "Newsletter",
                None,
                Some(verdict(Category::Newsletter)),
                2,
            ),
            row("c@x.com", "Hi", None, Some(verdict(Category::Personal)), 3),
            row("d@x.com", "Win", None, Some(verdict(Category::Phishing)), 4),
        ];
        let policy = config_two("").flag_policy();
        let default = Config::default();
        for derive in [None, Some(&policy)] {
            let digest = build(&DigestInput {
                account: "Example Home",
                date: jiff::civil::date(2026, 11, 7),
                period: (0, 100),
                rows: &rows,
                misfiled: &[],
                tz: &jiff::tz::TimeZone::UTC,
                flag_name: &|flag| default.flag_name(flag).to_owned(),
                flag_colour: &|flag| flag.colour(),
                derive_flags: derive,
                dry_run: false,
            });
            let counts = Counts::for_rows(&rows, derive);
            assert_eq!(digest.needs_you(), counts.to_look_at as usize, "{derive:?}");
            assert_eq!(
                digest.noise.bulk() + digest.noise.junk(),
                counts.done as usize,
                "{derive:?}",
            );
        }
    }

    // --- built report and text output ---

    #[test]
    fn text_output_names_engine_state_and_every_account() {
        let config = config_two("dry_run = false\n");
        let raw = RawStatus {
            engine: Some((900, 1000)),
            states: [
                ("Example Work".to_string(), "connected".to_string()),
                ("Example Home".to_string(), "sign_in_failed".to_string()),
            ]
            .to_vec(),
            rows: [
                [row(
                    "a@x.com",
                    "Reply",
                    Some(TriageFlag::ReplyNeeded),
                    None,
                    950,
                )]
                .to_vec(),
                Vec::<DigestRow>::new(),
            ]
            .to_vec(),
        };
        let report = build_report(1000, 900, &config, raw);
        let text = format_text(&report);
        assert!(text.contains("Engine: running"), "{text}");
        assert!(
            text.contains("Example Work (live): connected — Watching the inbox."),
            "{text}"
        );
        assert!(
            text.contains(
                "Example Home (classify-only): sign_in_failed — Sign-in failed. Check the password."
            ),
            "{text}"
        );
        assert!(text.contains("1 to look at, 0 fyi, 0 done"), "{text}");
    }

    #[test]
    fn config_order_and_total_counts_reach_the_report() {
        // The accounts array follows config order (the desktop never learns names any other
        // way), and the totals sum the per-account counts.
        let config = config_two("");
        let raw = RawStatus {
            engine: Some((1000, 1000)),
            states: Vec::<(String, String)>::new(),
            rows: [
                [row(
                    "a@x.com",
                    "Reply",
                    Some(TriageFlag::ReplyNeeded),
                    None,
                    950,
                )]
                .to_vec(),
                [row(
                    "b@x.com",
                    "Newsletter",
                    None,
                    Some(verdict(Category::Newsletter)),
                    960,
                )]
                .to_vec(),
            ]
            .to_vec(),
        };
        let report = build_report(1000, 950, &config, raw);
        let names = report
            .accounts
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Example Work", "Example Home"].to_vec());
        assert_eq!(
            report.counts,
            Counts {
                to_look_at: 1,
                fyi: 0,
                done: 1,
            },
        );
    }

    #[test]
    fn stored_states_are_read_leniently() {
        // A state this program never wrote reads as `failed`, not as a crash.
        let config = config_two("");
        let raw = RawStatus {
            engine: Some((990, 1000)),
            states: [("Example Work".to_string(), "quacking".to_string())].to_vec(),
            rows: [Vec::<DigestRow>::new(), Vec::<DigestRow>::new()].to_vec(),
        };
        let report = build_report(1000, 900, &config, raw);
        assert_eq!(report.accounts[0].state, AccountState::Failed);
        assert_eq!(report.accounts[0].reason, "Stopped after an error.");
    }

    #[test]
    fn json_contains_no_sender_or_subject() {
        // The metadata-only rule, made a regression test: a row that carries sender and subject
        // still counts toward the numbers, but neither string may reach the JSON. Distinct
        // sentinels so a leak could not hide behind an account name.
        let config = config_two("");
        let sender = "SENTINELSENDER@example.invalid";
        let subject = "SENTINELSUBLJECT-DO-NOT-LEAK";
        let raw = RawStatus {
            engine: Some((940, 1000)), // fresh heartbeat, so the engine reads as running
            states: [("Example Work".to_string(), "connected".to_string())].to_vec(),
            rows: [
                [
                    row(sender, subject, Some(TriageFlag::ReplyNeeded), None, 950),
                    row(
                        sender,
                        subject,
                        None,
                        Some(verdict(Category::Newsletter)),
                        951,
                    ),
                ]
                .to_vec(),
                Vec::<DigestRow>::new(),
            ]
            .to_vec(),
        };
        let report = build_report(1000, 950, &config, raw);
        // The rows are really counted (so the assertions below are not vacuously true).
        assert_eq!(report.counts.to_look_at, 1);
        assert_eq!(report.counts.done, 1);

        let value = serde_json::to_string(&report).unwrap();
        assert!(
            !value.contains(sender),
            "sender leaked into the status JSON"
        );
        assert!(
            !value.contains(subject),
            "subject leaked into the status JSON"
        );
        // And no mail metadata is a field this shape even has.
        for forbidden in ["sender", "subject", "body", "SENTINEL"] {
            assert!(
                !value.contains(forbidden),
                "{forbidden} appeared in the status JSON"
            );
        }
    }
}
