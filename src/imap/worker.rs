//! The account worker: watches one inbox and classifies new mail. Dry-run only for now.
//!
//! [`run_account`] runs until its [`CancellationToken`] fires. Its loop, per connection:
//!
//! 1. `SELECT` the inbox. Compare UIDVALIDITY and the saved position ([`plan_cursor`]).
//! 2. Fetch size and flags of every UID above the saved one, then process each message
//!    ([`Ctx::process_message`]).
//! 3. Retry earlier model failures that are due.
//! 4. Wait: `IDLE` (renewed every ~5 minutes, far below the 29-minute limit of RFC 2177), or
//!    a 60-second sleep if the server has no `IDLE`. Then start again at step 1.
//!
//! Re-selecting the mailbox at every wake-up is deliberate. It is cheap, it tells us the new
//! `UIDNEXT`, and it notices a changed UIDVALIDITY in one place, with no special code for
//! "the server renumbered the mailbox while we were idle".
//!
//! # Dry-run only
//!
//! Phase 4 adds the flagging. Until then **nothing in this file changes server state**:
//! [`run_account`] refuses to start when `dry_run = false`, and a source-scan test fails if the
//! flag-writing method of the session is ever mentioned here. Phase 4 plugs in at the single
//! place marked `PHASE 4` in [`Ctx::triage`].
//!
//! # Errors: what ends the worker
//!
//! Only cancellation ends the worker normally. It ends with an error when retrying cannot
//! help ([`WorkerError::is_fatal`]): a rejected login, a refused insecure connection, an
//! unusable TLS setup, a failing `password_command`, a broken database. Everything else
//! (network down, server hiccup, timeout, mailbox briefly unavailable) is logged at `warn`
//! and retried with exponential backoff. A rejected login is treated as fatal even though some
//! servers also say `NO` for a temporary overload: hammering a server with a wrong password
//! risks locking the account, and the log line tells the user what to fix.
//!
//! # Model failures
//!
//! A message whose classification fails is never guessed at. It goes into the `retry_queue`
//! table with a due time, and later wake-ups try it again with a growing delay
//! ([`backoff_delay`]). Retries run at wake-ups, so the real delay is "at least the backoff,
//! at most one more IDLE renewal".

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{IdleEnd, ImapError, ImapSession, ImapSettings, MessageMeta};
use crate::config::{AccountConfig, Config, ConfigError};
use crate::decider::{Decider, Evidence};
use crate::engine::{FlagPolicy, Trust, choose_flag, screen_sender};
use crate::message::{HEADERS_ONLY_THRESHOLD, ParsedMessage, parse_headers_only, parse_message};
use crate::precheck::{SOURCE_MODEL, SOURCE_RULE, precheck};
use crate::senders::plan_batches;
use crate::store::{LabelRecord, MailboxState, MessageKey, ProcessedRecord, Store, StoreError};

/// How many UIDs one metadata fetch covers.
const META_BATCH: u32 = 100;
/// How many due retries one wake-up handles, so a long outage cannot starve new mail.
const RETRIES_PER_WAKE: u32 = 20;

// ---------------------------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------------------------

/// Every interval the worker uses. Tests shorten them; production uses [`Default`].
#[derive(Debug, Clone)]
pub struct WorkerTiming {
    /// How long one `IDLE` lasts before it is re-issued.
    pub idle_renew: Duration,
    /// How often to look for mail when the server has no `IDLE`.
    pub poll_interval: Duration,
    /// First wait after a lost connection; doubles on every failure.
    pub reconnect_initial: Duration,
    /// Ceiling for the reconnect wait.
    pub reconnect_max: Duration,
    /// First wait before retrying a failed classification; doubles on every failure.
    pub retry_initial: Duration,
    /// Ceiling for the classification retry wait.
    pub retry_max: Duration,
}

impl Default for WorkerTiming {
    fn default() -> Self {
        WorkerTiming {
            idle_renew: Duration::from_secs(5 * 60),
            poll_interval: Duration::from_secs(60),
            reconnect_initial: Duration::from_secs(5),
            reconnect_max: Duration::from_secs(5 * 60),
            retry_initial: Duration::from_secs(60),
            retry_max: Duration::from_secs(60 * 60),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Why a worker ended or an attempt failed.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// `dry_run = false` was requested before phase 4 exists.
    #[error("flagging is not implemented yet (phase 4); set `policy.dry_run = true`")]
    FlaggingNotImplemented,
    /// The password could not be found.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Talking to the IMAP server failed.
    #[error(transparent)]
    Imap(#[from] ImapError),
    /// The database failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A blocking task panicked or was cancelled.
    #[error("a background task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl WorkerError {
    /// True when trying again cannot help, so the worker should stop and report.
    pub fn is_fatal(&self) -> bool {
        match self {
            WorkerError::Imap(error) => imap_error_is_fatal(error),
            // Retrying a missing password, a corrupt database or a panic would only repeat it.
            WorkerError::FlaggingNotImplemented
            | WorkerError::Config(_)
            | WorkerError::Store(_)
            | WorkerError::Task(_) => true,
        }
    }
}

/// Which IMAP errors are configuration problems rather than bad luck.
fn imap_error_is_fatal(error: &ImapError) -> bool {
    match error {
        ImapError::Login(_)
        | ImapError::InsecureTransport { .. }
        | ImapError::StartTlsUnsupported
        | ImapError::InvalidServerName(_)
        | ImapError::NoRootCertificates
        | ImapError::KeywordsNotAllowed => true,
        // Network, TLS handshake, timeouts and protocol hiccups come and go.
        ImapError::Tls(_)
        | ImapError::Io(_)
        | ImapError::Timeout
        | ImapError::Imap(_)
        | ImapError::NoMailboxSelected
        | ImapError::EmptyUidSet
        | ImapError::MissingData(_) => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// Exponential backoff: `initial` for the first failure, doubled for each further one, never
/// more than `max`. `attempt` counts failures and starts at 1 (0 is treated as 1).
///
/// Used for reconnecting (5 s, 10 s, ... 5 min) and for classification retries (1 min, 2 min,
/// ... 1 h). `saturating_*` and the cap on the shift keep it overflow-free for any attempt.
pub fn backoff_delay(attempt: u32, initial: Duration, max: Duration) -> Duration {
    let doublings = attempt.saturating_sub(1).min(30);
    initial.saturating_mul(1u32 << doublings).min(max)
}

/// What happened to the saved position of the inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorChange {
    /// Same UIDVALIDITY as before; carry on from the saved UID.
    Unchanged,
    /// The mailbox was never seen: start at `UIDNEXT - 1` and leave the history alone.
    FirstRun,
    /// The server renumbered the mailbox. Old UIDs mean nothing; start from the new end.
    UidValidityChanged,
}

/// Where to continue, given the saved position and what `SELECT` just reported.
///
/// On the first run and after a UIDVALIDITY change the position is `UIDNEXT - 1`, the newest
/// message that exists: mail already in the inbox is history, and re-classifying it all would
/// be slow and surprising. (`senders::resume_point` restarts at 0 instead, because reading old
/// Sent mail twice is harmless.)
pub fn plan_cursor(
    saved: Option<MailboxState>,
    uid_validity: u32,
    uid_next: u32,
) -> (MailboxState, CursorChange) {
    match saved {
        Some(state) if state.uidvalidity == uid_validity => (state, CursorChange::Unchanged),
        Some(_) => (
            MailboxState {
                uidvalidity: uid_validity,
                last_uid: uid_next.saturating_sub(1),
            },
            CursorChange::UidValidityChanged,
        ),
        None => (
            MailboxState {
                uidvalidity: uid_validity,
                last_uid: uid_next.saturating_sub(1),
            },
            CursorChange::FirstRun,
        ),
    }
}

/// How much of a message to download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchPlan {
    /// `BODY.PEEK[]`: the whole message.
    Full,
    /// `BODY.PEEK[HEADER]`: too big to download for classification.
    HeadersOnly,
}

/// Decides from `RFC822.SIZE` how much to fetch: up to 2 MB in full, otherwise headers only.
/// An unknown size also gets headers only; every real server reports it, so "unknown" is
/// unusual, and the cautious choice costs a little accuracy instead of memory.
pub fn fetch_plan(size: Option<u32>) -> FetchPlan {
    match size {
        Some(size) if (size as usize) <= HEADERS_ONLY_THRESHOLD => FetchPlan::Full,
        _ => FetchPlan::HeadersOnly,
    }
}

/// Why a message is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The message already carries `\Flagged` (any colour): the user got there first.
    UserFlagged,
    /// This exact message (same UIDVALIDITY and UID) was handled before.
    AlreadyProcessed,
    /// The message comes from the account's own address.
    OwnAddress,
    /// The bytes could not be parsed as a message.
    Unparseable,
}

/// The skip checks that need only the metadata, so no download is wasted.
///
/// Any `\Flagged` counts, not only the seven colours: the service never sets a flag in
/// dry-run, and later it records what it set, so a flag seen here is the user's.
pub fn skip_before_fetch(flags: &[String], already_processed: bool) -> Option<SkipReason> {
    if flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case(crate::flags::FLAGGED))
    {
        Some(SkipReason::UserFlagged)
    } else if already_processed {
        Some(SkipReason::AlreadyProcessed)
    } else {
        None
    }
}

/// The account's own address: the login name, when it looks like an email address.
/// (An alias list in the config is an open question.)
pub fn own_address(account: &AccountConfig) -> Option<String> {
    let username = account.username.trim().to_lowercase();
    username.contains('@').then_some(username)
}

/// True when `from` (already lowercase) is the account's own address.
pub fn is_own_message(from: &str, own: Option<&str>) -> bool {
    own.is_some_and(|own| own == from)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

// ---------------------------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------------------------

/// What happened to one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Left alone, for the given reason.
    Skipped(SkipReason),
    /// Classified (or blocked by a rule) and recorded.
    Processed,
    /// The model failed; the message is in the retry queue.
    Deferred,
    /// The message disappeared from the server before we could read it.
    Gone,
    /// Shutdown arrived while the model was working. Nothing was recorded.
    Cancelled,
}

/// Whether a loop should go on.
type Flow = ControlFlow<()>;

/// Runs the worker for one account until `shutdown` fires, using the default timing.
///
/// Returns `Ok(())` on cancellation and `Err` for a fatal error (see the module docs).
/// The decider and the store are shared with other workers, hence `Arc`.
pub async fn run_account<D: Decider>(
    account: &AccountConfig,
    config: &Config,
    decider: Arc<D>,
    store: Arc<Mutex<Store>>,
    shutdown: CancellationToken,
) -> Result<(), WorkerError> {
    run_account_with_timing(
        account,
        config,
        decider,
        store,
        shutdown,
        WorkerTiming::default(),
    )
    .await
}

/// Same as [`run_account`], with custom intervals. Tests use short ones.
pub async fn run_account_with_timing<D: Decider>(
    account: &AccountConfig,
    config: &Config,
    decider: Arc<D>,
    store: Arc<Mutex<Store>>,
    shutdown: CancellationToken,
    timing: WorkerTiming,
) -> Result<(), WorkerError> {
    // PHASE 4: remove this guard when flagging exists. Until then it is impossible to reach
    // any state-changing IMAP command from here.
    if !config.policy.dry_run {
        return Err(WorkerError::FlaggingNotImplemented);
    }

    // `password_command` runs a shell command and waits for it, which would block the async
    // runtime, so it goes to the blocking pool. It runs once; reconnects reuse the result, so
    // a flapping network does not pop up a keychain prompt every few seconds.
    let for_password = account.clone();
    let password = tokio::task::spawn_blocking(move || for_password.password()).await??;
    let settings = account.imap_settings(&password);

    let ctx = Ctx {
        account,
        config,
        policy: config.flag_policy(),
        decider: decider.as_ref(),
        store: &store,
        shutdown: &shutdown,
        timing: &timing,
        own: own_address(account),
    };

    let mut failures = 0u32;
    loop {
        match ctx.serve(&settings, &mut failures).await {
            Ok(()) => return Ok(()),
            Err(error) if error.is_fatal() => {
                tracing::error!(account = %account.name, %error, "worker stopped: cannot be fixed by retrying");
                return Err(error);
            }
            Err(error) => {
                failures += 1;
                let delay = backoff_delay(failures, timing.reconnect_initial, timing.reconnect_max);
                tracing::warn!(
                    account = %account.name, %error, retry_in_secs = delay.as_secs(),
                    "connection lost or failed; reconnecting"
                );
                if shutdown
                    .run_until_cancelled(tokio::time::sleep(delay))
                    .await
                    .is_none()
                {
                    return Ok(());
                }
            }
        }
    }
}

/// Everything one worker needs, borrowed from [`run_account_with_timing`]'s frame.
///
/// The `'a` is a lifetime parameter: the struct holds references, and the compiler checks that
/// none of them outlives what it points to. Bundling them avoids passing eight arguments to
/// every method.
struct Ctx<'a, D> {
    account: &'a AccountConfig,
    config: &'a Config,
    policy: FlagPolicy,
    decider: &'a D,
    store: &'a Arc<Mutex<Store>>,
    shutdown: &'a CancellationToken,
    timing: &'a WorkerTiming,
    own: Option<String>,
}

impl<D: Decider> Ctx<'_, D> {
    /// One connection's lifetime. `Ok(())` means shutdown; an error means "reconnect" unless
    /// it is fatal. `failures` is reset to 0 as soon as the login works, so the backoff starts
    /// over after every successful connection.
    async fn serve(&self, settings: &ImapSettings, failures: &mut u32) -> Result<(), WorkerError> {
        let Some(connected) = self
            .shutdown
            .run_until_cancelled(ImapSession::connect(settings))
            .await
        else {
            return Ok(());
        };
        let mut session = connected?;
        *failures = 0;

        let use_idle = session.capabilities().await?.idle;
        tracing::info!(account = %self.account.name, idle = use_idle, "connected");

        let inbox = self.account.inbox.as_str();
        let name = self.account.name.clone();
        let mailbox = inbox.to_owned();
        let mut saved = self
            .with_store(move |store| store.mailbox_state(&name, &mailbox))
            .await?;

        loop {
            let info = session.select(inbox).await?;

            let (mut state, change) = plan_cursor(saved, info.uid_validity, info.uid_next);
            match change {
                CursorChange::Unchanged => {}
                CursorChange::FirstRun => tracing::info!(
                    account = %self.account.name, last_uid = state.last_uid,
                    "first run: leaving the existing inbox alone"
                ),
                CursorChange::UidValidityChanged => tracing::warn!(
                    account = %self.account.name, new_uidvalidity = state.uidvalidity,
                    last_uid = state.last_uid,
                    "UIDVALIDITY changed: the server renumbered the inbox; starting from the newest message"
                ),
            }
            if change != CursorChange::Unchanged {
                self.save_cursor(state).await?;
            }

            let flow = self
                .catch_up(&mut session, &mut state, info.uid_next)
                .await?;
            saved = Some(state);
            if flow.is_break() {
                return finish(session).await;
            }
            if self
                .run_retries(&mut session, state.uidvalidity)
                .await?
                .is_break()
            {
                return finish(session).await;
            }

            if use_idle {
                let (back, end) = session.idle(self.timing.idle_renew, self.shutdown).await?;
                session = back;
                if end == IdleEnd::Cancelled {
                    return finish(session).await;
                }
            } else if self
                .shutdown
                .run_until_cancelled(tokio::time::sleep(self.timing.poll_interval))
                .await
                .is_none()
            {
                return finish(session).await;
            }
        }
    }

    /// Process every message above the saved UID, saving the position after each one so a
    /// restart resumes where this stopped.
    async fn catch_up(
        &self,
        session: &mut ImapSession,
        state: &mut MailboxState,
        uid_next: u32,
    ) -> Result<Flow, WorkerError> {
        for (first, last) in plan_batches(state.last_uid, uid_next, META_BATCH) {
            for meta in session.fetch_meta_range(first, last).await? {
                if self.shutdown.is_cancelled() {
                    return Ok(ControlFlow::Break(()));
                }
                let key = self.key(state.uidvalidity, meta.uid);
                let outcome = self.process_message(session, &meta, &key, 0).await?;
                if outcome == Outcome::Cancelled {
                    return Ok(ControlFlow::Break(()));
                }
                state.last_uid = meta.uid;
                self.save_cursor(*state).await?;
            }
            // UIDs in the range that no longer exist are simply passed over.
            state.last_uid = last;
            self.save_cursor(*state).await?;
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Try again the messages whose classification failed earlier and whose time has come.
    async fn run_retries(
        &self,
        session: &mut ImapSession,
        uid_validity: u32,
    ) -> Result<Flow, WorkerError> {
        let name = self.account.name.clone();
        let due = self
            .with_store(move |store| store.due_retries(&name, now_unix(), RETRIES_PER_WAKE))
            .await?;

        for item in due {
            if self.shutdown.is_cancelled() {
                return Ok(ControlFlow::Break(()));
            }
            // A queue entry for another mailbox or numbering cannot be acted on safely.
            let stale =
                item.key.mailbox != self.account.inbox || item.key.uidvalidity != uid_validity;
            let meta = if stale {
                None
            } else {
                session
                    .fetch_meta_range(item.key.uid, item.key.uid)
                    .await?
                    .into_iter()
                    .next()
            };
            let outcome = match &meta {
                Some(meta) => {
                    self.process_message(session, meta, &item.key, item.attempts)
                        .await?
                }
                None => Outcome::Gone,
            };
            match outcome {
                Outcome::Cancelled => return Ok(ControlFlow::Break(())),
                // Failed again: `triage` already re-queued it with a longer delay.
                Outcome::Deferred => {}
                _ => {
                    let id = item.id;
                    self.with_store(move |store| store.remove_retry(id)).await?;
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Fetch one message (as little as needed) and run the pipeline on it.
    /// `failed_attempts` is how often the model already failed for it (0 for new mail).
    async fn process_message(
        &self,
        session: &mut ImapSession,
        meta: &MessageMeta,
        key: &MessageKey,
        failed_attempts: u32,
    ) -> Result<Outcome, WorkerError> {
        let lookup = key.clone();
        let processed = self
            .with_store(move |store| store.is_processed(&lookup))
            .await?;
        if let Some(reason) = skip_before_fetch(&meta.flags, processed) {
            tracing::debug!(account = %self.account.name, uid = meta.uid, ?reason, "skipping");
            return Ok(Outcome::Skipped(reason));
        }

        let plan = fetch_plan(meta.size);
        let raw = match plan {
            FetchPlan::Full => session.fetch_body_peek(meta.uid).await?,
            FetchPlan::HeadersOnly => session.fetch_headers_peek(meta.uid).await?,
        };
        let Some(raw) = raw else {
            return Ok(Outcome::Gone);
        };

        let authserv = Some(self.config.policy.authserv_id.as_str()).filter(|id| !id.is_empty());
        let provider = self.config.policy.provider_headers;
        let parsed = match plan {
            FetchPlan::Full => parse_message(&raw, authserv, provider),
            FetchPlan::HeadersOnly => parse_headers_only(&raw, authserv, provider),
        };
        let Ok(parsed) = parsed else {
            tracing::warn!(account = %self.account.name, uid = meta.uid, "cannot parse the message; skipping");
            return Ok(Outcome::Skipped(SkipReason::Unparseable));
        };

        self.triage(key, &parsed, failed_attempts).await
    }

    /// The pipeline after parsing: own-address skip, rule and known-sender lookup,
    /// `screen_sender`, decider, `choose_flag`, record. No IMAP session needed, which is what
    /// lets the unit tests run it with a fake decider and an in-memory database.
    async fn triage(
        &self,
        key: &MessageKey,
        parsed: &ParsedMessage,
        failed_attempts: u32,
    ) -> Result<Outcome, WorkerError> {
        if is_own_message(&parsed.from_address, self.own.as_deref()) {
            return Ok(Outcome::Skipped(SkipReason::OwnAddress));
        }

        let (name, sender, domain) = (
            self.account.name.clone(),
            parsed.from_address.clone(),
            parsed.from_domain().to_owned(),
        );
        let (rule, known_sender, domain_count) = self
            .with_store(move |store| {
                Ok((
                    store.match_rule(&name, &sender)?,
                    store.is_known_sender(&name, &sender)?,
                    store.known_domain_count(&name, &domain)?,
                ))
            })
            .await?;
        let trust = screen_sender(rule, known_sender, parsed.auth.any_fail());

        let base = ProcessedRecord {
            key: key.clone(),
            message_id: parsed.message_id.clone(),
            sender: Some(parsed.from_address.clone()),
            subject: Some(parsed.subject.clone()),
            verdict: None,
            flag: None,
            source: String::from(SOURCE_RULE),
        };

        // A blocked sender is never shown to the model.
        if trust == Trust::Blocked {
            self.with_store(move |store| store.record_processed(&base))
                .await?;
            tracing::info!(account = %self.account.name, uid = key.uid, "dry-run: blocked sender, no flag");
            return Ok(Outcome::Processed);
        }

        let evidence = Evidence::from_message(parsed, domain_count > 0);

        // Certain spam is decided by code and the model is never asked. A sender the user
        // allowed or has written to (`Trusted`) is exempt: the heuristics must not override
        // the user's own knowledge.
        let rule_verdict = if trust == Trust::Unknown {
            precheck(&evidence)
        } else {
            None
        };
        let (verdict, source) = match rule_verdict {
            Some(verdict) => (verdict, SOURCE_RULE),
            None => {
                let Some(decision) = self
                    .shutdown
                    .run_until_cancelled(self.decider.decide(&evidence))
                    .await
                else {
                    return Ok(Outcome::Cancelled);
                };
                match decision {
                    Ok(verdict) => (verdict, SOURCE_MODEL),
                    Err(error) => {
                        // No guess, no action: queue it for later.
                        let attempts = failed_attempts + 1;
                        let delay = backoff_delay(
                            attempts,
                            self.timing.retry_initial,
                            self.timing.retry_max,
                        );
                        let due = now_unix()
                            .saturating_add(i64::try_from(delay.as_secs()).unwrap_or(i64::MAX));
                        let (retry_key, text) = (key.clone(), error.to_string());
                        self.with_store(move |store| store.enqueue_retry(&retry_key, &text, due))
                            .await?;
                        tracing::warn!(
                            account = %self.account.name, uid = key.uid, attempts,
                            retry_in_secs = delay.as_secs(), %error,
                            "the model failed; message queued for a retry"
                        );
                        return Ok(Outcome::Deferred);
                    }
                }
            }
        };

        let flag = choose_flag(trust, &verdict, &self.policy);

        // PHASE 4: `dry_run` is always true here (see `run_account_with_timing`). This is the
        // one place where flagging plugs in: when `dry_run` is false, send the STORE for
        // `flag` first and write the record below only if it succeeded.
        tracing::info!(
            account = %self.account.name,
            uid = key.uid,
            category = verdict.category.as_str(),
            source,
            flag = ?flag.map(|flag| self.config.flag_name(flag)),
            colour = ?flag.map(|flag| self.policy.colour_for(flag)),
            "dry-run: would set flag (nothing was changed on the server)"
        );

        let record = ProcessedRecord {
            verdict: Some(verdict.clone()),
            flag,
            source: String::from(source),
            ..base
        };
        let label = LabelRecord {
            key: key.clone(),
            features: features_json(&evidence, trust, parsed.from_domain()),
            verdict,
            flag,
        };
        self.with_store(move |store| {
            store.record_processed(&record)?;
            store.record_label(&label)
        })
        .await?;
        Ok(Outcome::Processed)
    }

    fn key(&self, uidvalidity: u32, uid: u32) -> MessageKey {
        MessageKey {
            account: self.account.name.clone(),
            mailbox: self.account.inbox.clone(),
            uidvalidity,
            uid,
        }
    }

    async fn save_cursor(&self, state: MailboxState) -> Result<(), WorkerError> {
        let (name, mailbox) = (self.account.name.clone(), self.account.inbox.clone());
        self.with_store(move |store| {
            store
                .set_mailbox_state(&name, &mailbox, state.uidvalidity, state.last_uid)
                .map(|_reset| ())
        })
        .await
    }

    /// Run a synchronous store operation on tokio's blocking pool, as `senders.rs` does.
    ///
    /// The lock is taken and released inside the blocking closure, so it is never held across
    /// an `.await`. A poisoned lock (a thread panicked while holding it) is recovered:
    /// SQLite rolls back a half-done transaction on its own.
    async fn with_store<T, F>(&self, work: F) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    {
        let store = Arc::clone(self.store);
        let result = tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().unwrap_or_else(PoisonError::into_inner);
            work(&mut guard)
        })
        .await?;
        Ok(result?)
    }
}

/// Log out politely after a shutdown request. A failure here does not matter: we are leaving.
async fn finish(session: ImapSession) -> Result<(), WorkerError> {
    let _ = session.logout().await;
    Ok(())
}

/// The evidence features stored with a label: header facts and counts, never text from the
/// message (no subject, no body, no names).
fn features_json(evidence: &Evidence, trust: Trust, sender_domain: &str) -> String {
    json!({
        "sender_domain": sender_domain,
        "trust": format!("{trust:?}").to_lowercase(),
        "known_domain": evidence.known_domain,
        "bulk_headers": evidence.bulk_headers,
        "auto_submitted": evidence.auto_submitted,
        "reply_to_mismatch": evidence.reply_to_mismatch,
        "display_name_spoof": evidence.display_name_spoof,
        "malformed_sender": evidence.malformed_sender,
        "impersonates_recipient": evidence.impersonates_recipient,
        "provider_spam": evidence.provider_spam,
        "spf": format!("{:?}", evidence.spf).to_lowercase(),
        "dkim": format!("{:?}", evidence.dkim).to_lowercase(),
        "dmarc": format!("{:?}", evidence.dmarc).to_lowercase(),
        "link_count": evidence.link_count,
        "attachments": evidence.attachment_names.len(),
        "headers_only": evidence.headers_only,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decider::DecideError;
    use crate::engine::{Category, Payment, Rule, Verdict};
    use std::sync::atomic::{AtomicU32, Ordering};

    const SECS: fn(u64) -> Duration = Duration::from_secs;

    // ---- pure functions ----------------------------------------------------------------

    #[test]
    fn backoff_doubles_and_is_capped() {
        let delays: Vec<u64> = (1..=8)
            .map(|n| backoff_delay(n, SECS(60), SECS(3600)).as_secs())
            .collect();
        assert_eq!(delays, [60, 120, 240, 480, 960, 1920, 3600, 3600]);
        assert_eq!(backoff_delay(1, SECS(5), SECS(300)), SECS(5));
        assert_eq!(backoff_delay(7, SECS(5), SECS(300)), SECS(300));
    }

    #[test]
    fn backoff_survives_huge_attempt_counts() {
        assert_eq!(backoff_delay(u32::MAX, SECS(5), SECS(300)), SECS(300));
        assert_eq!(backoff_delay(0, SECS(5), SECS(300)), SECS(5));
    }

    #[test]
    fn first_run_starts_at_uidnext_minus_one() {
        let (state, change) = plan_cursor(None, 7, 43);
        assert_eq!((state.uidvalidity, state.last_uid), (7, 42));
        assert_eq!(change, CursorChange::FirstRun);
        // An empty mailbox has UIDNEXT 1, so nothing has been seen.
        assert_eq!(plan_cursor(None, 7, 1).0.last_uid, 0);
    }

    #[test]
    fn matching_uidvalidity_keeps_the_saved_position() {
        let saved = MailboxState {
            uidvalidity: 7,
            last_uid: 30,
        };
        let (state, change) = plan_cursor(Some(saved), 7, 43);
        assert_eq!(state, saved);
        assert_eq!(change, CursorChange::Unchanged);
    }

    #[test]
    fn changed_uidvalidity_resets_to_the_new_end_not_to_zero() {
        let saved = MailboxState {
            uidvalidity: 7,
            last_uid: 30,
        };
        let (state, change) = plan_cursor(Some(saved), 8, 5);
        assert_eq!((state.uidvalidity, state.last_uid), (8, 4));
        assert_eq!(change, CursorChange::UidValidityChanged);
    }

    #[test]
    fn fetch_plan_follows_the_two_megabyte_threshold() {
        let limit = u32::try_from(HEADERS_ONLY_THRESHOLD).unwrap();
        assert_eq!(fetch_plan(Some(0)), FetchPlan::Full);
        assert_eq!(fetch_plan(Some(limit)), FetchPlan::Full);
        assert_eq!(fetch_plan(Some(limit + 1)), FetchPlan::HeadersOnly);
        assert_eq!(fetch_plan(None), FetchPlan::HeadersOnly);
    }

    fn flags(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[test]
    fn any_flagged_message_is_the_users() {
        let skip = |names: &[&str], processed| skip_before_fetch(&flags(names), processed);
        assert_eq!(skip(&["\\Flagged"], false), Some(SkipReason::UserFlagged));
        assert_eq!(
            skip(&["\\flagged", "$MailFlagBit0"], false),
            Some(SkipReason::UserFlagged)
        );
        // Flagged with the unassigned 1-1-1 bits still counts: the user did something.
        assert_eq!(
            skip(
                &[
                    "\\Flagged",
                    "$MailFlagBit0",
                    "$MailFlagBit1",
                    "$MailFlagBit2"
                ],
                false
            ),
            Some(SkipReason::UserFlagged)
        );
        // Colour bits without \Flagged are ignored (RFC 9979), and \Seen is not a reason.
        assert_eq!(skip(&["$MailFlagBit0", "\\Seen"], false), None);
        assert_eq!(skip(&[], false), None);
    }

    #[test]
    fn processed_messages_are_skipped() {
        assert_eq!(
            skip_before_fetch(&[], true),
            Some(SkipReason::AlreadyProcessed)
        );
        // The user's flag is reported first.
        assert_eq!(
            skip_before_fetch(&flags(&["\\Flagged"]), true),
            Some(SkipReason::UserFlagged)
        );
    }

    fn account(username: &str) -> AccountConfig {
        toml::from_str(&format!(
            "name = \"test\"\nhost = \"127.0.0.1\"\nusername = \"{username}\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn own_address_needs_an_email_shaped_username() {
        assert_eq!(
            own_address(&account("Me@Example.org")).as_deref(),
            Some("me@example.org")
        );
        assert_eq!(own_address(&account("piotr")), None);
        assert!(is_own_message("me@example.org", Some("me@example.org")));
        assert!(!is_own_message("you@example.org", Some("me@example.org")));
        assert!(!is_own_message("me@example.org", None));
    }

    #[test]
    fn configuration_errors_are_fatal_and_network_errors_are_not() {
        let fatal = [
            ImapError::InsecureTransport { host: "x".into() },
            ImapError::StartTlsUnsupported,
            ImapError::InvalidServerName("x".into()),
            ImapError::NoRootCertificates,
        ];
        for error in fatal {
            assert!(imap_error_is_fatal(&error), "{error}");
        }
        let transient = [
            ImapError::Timeout,
            ImapError::Io(std::io::Error::other("reset")),
            ImapError::Tls("handshake".into()),
            ImapError::MissingData("UIDNEXT"),
        ];
        for error in transient {
            assert!(!imap_error_is_fatal(&error), "{error}");
        }
        assert!(WorkerError::FlaggingNotImplemented.is_fatal());
    }

    /// Hard rule: no state-changing IMAP command in the dry-run worker. The forbidden words
    /// are assembled at runtime so this test does not match itself.
    #[test]
    fn worker_source_has_no_state_changing_calls() {
        let production_code = include_str!("worker.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["store", "_flags"].concat(),
            ["uid", "_store"].concat(),
            [".app", "end("].concat(),
            [".exp", "unge("].concat(),
            [".uid", "_mv("].concat(),
            [".uid", "_copy("].concat(),
        ] {
            assert!(
                !production_code.contains(&forbidden),
                "the dry-run worker must not use {forbidden}"
            );
        }
    }

    // ---- the pipeline with a fake decider ----------------------------------------------

    /// A decider that returns a canned answer and counts its calls.
    struct FakeDecider {
        verdict: Option<Verdict>,
        calls: AtomicU32,
    }

    impl FakeDecider {
        fn answering(verdict: Verdict) -> FakeDecider {
            FakeDecider {
                verdict: Some(verdict),
                calls: AtomicU32::new(0),
            }
        }
        fn failing() -> FakeDecider {
            FakeDecider {
                verdict: None,
                calls: AtomicU32::new(0),
            }
        }
    }

    impl Decider for FakeDecider {
        async fn decide(&self, _evidence: &Evidence) -> Result<Verdict, DecideError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.verdict
                .clone()
                .ok_or_else(|| DecideError::InvalidVerdict("fake failure".into()))
        }
    }

    fn reply_needed() -> Verdict {
        Verdict {
            category: Category::Personal,
            needs_reply: true,
            deadline: None,
            payment: Payment::NotApplicable,
            confidence: 0.95,
            reason: "asks a question".into(),
        }
    }

    fn parsed(from: &str) -> ParsedMessage {
        let raw = format!(
            "From: Alice <{from}>\r\nTo: me@example.org\r\nSubject: Lunch?\r\n\
             Message-ID: <1@example.org>\r\n\r\nAre you free on Friday?\r\n"
        );
        parse_message(raw.as_bytes(), None, None).unwrap()
    }

    fn key(uid: u32) -> MessageKey {
        MessageKey {
            account: "test".into(),
            mailbox: "INBOX".into(),
            uidvalidity: 1,
            uid,
        }
    }

    /// Runs `triage` once with the given decider and returns the outcome and the store.
    async fn run_triage(
        decider: &FakeDecider,
        prepare: impl FnOnce(&Store),
        username: &str,
        from: &str,
    ) -> (Outcome, Arc<Mutex<Store>>) {
        let store = Store::open_in_memory().unwrap();
        prepare(&store);
        let store = Arc::new(Mutex::new(store));
        let (account, config) = (account(username), Config::default());
        let (shutdown, timing) = (CancellationToken::new(), WorkerTiming::default());
        let ctx = Ctx {
            account: &account,
            config: &config,
            policy: config.flag_policy(),
            decider,
            store: &store,
            shutdown: &shutdown,
            timing: &timing,
            own: own_address(&account),
        };
        let outcome = ctx.triage(&key(5), &parsed(from), 0).await.unwrap();
        (outcome, store)
    }

    #[tokio::test]
    async fn a_classified_message_is_recorded_with_its_flag() {
        let decider = FakeDecider::answering(reply_needed());
        let (outcome, store) =
            run_triage(&decider, |_| {}, "me@example.org", "anna@friend.example").await;
        assert_eq!(outcome, Outcome::Processed);
        let store = store.lock().unwrap();
        assert!(store.is_processed(&key(5)).unwrap());
        let counts = store.flag_counts(0).unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts[0].label, "reply_needed");
        // Nothing was queued for a retry.
        assert!(store.due_retries("test", i64::MAX, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_model_failure_queues_a_retry_and_records_nothing() {
        let decider = FakeDecider::failing();
        let before = now_unix();
        let (outcome, store) =
            run_triage(&decider, |_| {}, "me@example.org", "anna@friend.example").await;
        assert_eq!(outcome, Outcome::Deferred);
        let store = store.lock().unwrap();
        assert!(!store.is_processed(&key(5)).unwrap());
        let queued = store.due_retries("test", i64::MAX, 10).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].key, key(5));
        assert_eq!(queued[0].attempts, 1);
        // First failure waits the initial minute.
        assert!(queued[0].next_attempt_at >= before + 60);
    }

    #[tokio::test]
    async fn a_model_verdict_is_recorded_with_source_model() {
        let decider = FakeDecider::answering(reply_needed());
        let (_, store) =
            run_triage(&decider, |_| {}, "me@example.org", "anna@friend.example").await;
        let source = store.lock().unwrap().processed_source(&key(5)).unwrap();
        assert_eq!(source.as_deref(), Some("model"));
    }

    #[tokio::test]
    async fn certain_spam_is_decided_by_the_rule_without_calling_the_model() {
        let decider = FakeDecider::answering(reply_needed());
        // `..` in the domain: an invalid sender address.
        let (outcome, store) = run_triage(
            &decider,
            |_| {},
            "me@example.org",
            "me.x7k2.qwrty@volny.example..sn.live.example",
        )
        .await;
        assert_eq!(outcome, Outcome::Processed);
        assert_eq!(
            decider.calls.load(Ordering::SeqCst),
            0,
            "the model was asked"
        );
        let store = store.lock().unwrap();
        assert_eq!(
            store.processed_source(&key(5)).unwrap().as_deref(),
            Some("rule")
        );
        // The record carries the spam verdict, and spam is never flagged.
        assert!(store.flag_counts(0).unwrap().is_empty());
        let categories = store.category_counts(0).unwrap();
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].label, "spam");
    }

    #[tokio::test]
    async fn a_trusted_sender_is_exempt_from_the_rules() {
        let decider = FakeDecider::answering(reply_needed());
        let (_, store) = run_triage(
            &decider,
            |store| {
                store
                    .add_rule(
                        "test",
                        "me.x7k2.qwrty@volny.example..sn.live.example",
                        Rule::Allow,
                    )
                    .unwrap();
            },
            "me@example.org",
            "me.x7k2.qwrty@volny.example..sn.live.example",
        )
        .await;
        assert_eq!(decider.calls.load(Ordering::SeqCst), 1);
        let source = store.lock().unwrap().processed_source(&key(5)).unwrap();
        assert_eq!(source.as_deref(), Some("model"));
    }

    #[tokio::test]
    async fn a_blocked_sender_never_reaches_the_model() {
        let decider = FakeDecider::answering(reply_needed());
        let (outcome, store) = run_triage(
            &decider,
            |store| {
                store
                    .add_rule("test", "@spam.example", Rule::Block)
                    .unwrap()
            },
            "me@example.org",
            "bad@spam.example",
        )
        .await;
        assert_eq!(outcome, Outcome::Processed);
        assert_eq!(decider.calls.load(Ordering::SeqCst), 0);
        assert!(store.lock().unwrap().is_processed(&key(5)).unwrap());
    }

    #[tokio::test]
    async fn the_accounts_own_mail_is_skipped() {
        let decider = FakeDecider::answering(reply_needed());
        let (outcome, store) =
            run_triage(&decider, |_| {}, "me@example.org", "ME@example.org").await;
        assert_eq!(outcome, Outcome::Skipped(SkipReason::OwnAddress));
        assert_eq!(decider.calls.load(Ordering::SeqCst), 0);
        assert!(!store.lock().unwrap().is_processed(&key(5)).unwrap());
    }

    #[tokio::test]
    async fn flagging_is_refused_until_phase_4() {
        let account = account("me@example.org");
        let mut config = Config::default();
        config.policy.dry_run = false;
        let decider = Arc::new(FakeDecider::failing());
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let result = run_account(&account, &config, decider, store, CancellationToken::new()).await;
        assert!(matches!(result, Err(WorkerError::FlaggingNotImplemented)));
    }
}
