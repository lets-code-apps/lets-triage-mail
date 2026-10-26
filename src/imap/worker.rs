//! The account worker: watches one inbox, classifies new mail, and sets and clears our flags.
//!
//! [`run_account`] runs until its [`CancellationToken`] fires. Its loop, per connection:
//!
//! 1. `SELECT` the inbox. Compare UIDVALIDITY and the saved position ([`plan_cursor`]).
//! 2. Fetch size and flags of every UID above the saved one, then process each message
//!    ([`Ctx::process_message`]).
//! 3. Retry earlier model failures that are due.
//! 4. Now and then (every [`WorkerTiming::clear_interval`]) run the auto-clear pass
//!    ([`Ctx::clear_pass`]) over the flags we set earlier.
//! 5. Wait: `IDLE` (renewed every ~5 minutes, far below the 29-minute limit of RFC 2177), or
//!    a 60-second sleep if the server has no `IDLE`. Then start again at step 1.
//!
//! Re-selecting the mailbox at every wake-up is deliberate. It is cheap, it tells us the new
//! `UIDNEXT`, and it notices a changed UIDVALIDITY in one place, with no special code for
//! "the server renumbered the mailbox while we were idle".
//!
//! # What may change on the server
//!
//! Only flags, and only ours. This file has exactly one way to write: a `UID STORE` of flag
//! keywords through [`FlagBackend::change_flags`], used in two places: setting a colour
//! ([`Ctx::set_flag`]) and removing one we set earlier ([`Ctx::review_owned`]). A source-scan
//! test fails if anything that moves, copies, appends or deletes mail is ever mentioned here.
//!
//! Whether the service writes at all is decided per account by
//! [`AccountConfig::flag_mode`]: `Live` writes, `DryRun` only logs what it would do, and
//! `ClassifyOnly` (`flagging = false`) never chooses a flag. A live account must also have
//! custom keywords (see [`ensure_keywords`]), or the worker refuses to start: a bare
//! `\Flagged` shows as red in Apple Mail, and red is the user's.
//!
//! The decisions themselves (may we set this colour, is this flag still ours, should it clear)
//! are pure functions in [`crate::flagging`]; this file gathers their inputs and carries out
//! the answer.
//!
//! # The auto-clear pass: when it runs
//!
//! It runs on the worker's own wake-ups, throttled to once per `clear_interval` (15 minutes),
//! instead of on a separate timer or connection. Reasons: the worker's connection is already
//! open and selected on the inbox; one task means no two STOREs can race on the same message;
//! and nothing here is urgent, since a flag that clears a few minutes late costs nothing. The
//! price is that the pass can only start when the worker wakes (at the latest every IDLE
//! renewal, 5 minutes).
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

use std::collections::HashMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jiff::civil::Date;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{IdleEnd, ImapError, ImapSession, ImapSettings, MessageMeta};
use crate::config::{AccountConfig, Config, ConfigError, FlagMode};
use crate::decider::{Decider, Evidence};
use crate::engine::{FlagPolicy, TriageFlag, Trust, Verdict, choose_flag, screen_sender};
use crate::flagging::{
    ApplyPlan, LeaveReason, OwnedState, clear_reason, normalise_message_id, owned_state, plan_apply,
};
use crate::flags::{StoreChange, clear_change};
use crate::message::{HEADERS_ONLY_THRESHOLD, ParsedMessage, parse_headers_only, parse_message};
use crate::precheck::{SOURCE_MODEL, SOURCE_RULE, precheck};
use crate::senders::{HarvestError, harvest_known_senders, plan_batches};
use crate::store::{
    LabelRecord, MailboxState, MessageKey, OwnedFlag, ProcessedRecord, Store, StoreError,
};

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
    /// How often the auto-clear pass looks at the flags we set. It runs at a wake-up, so the
    /// real gap is "at least this, at most one more IDLE renewal".
    pub clear_interval: Duration,
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
            clear_interval: Duration::from_secs(15 * 60),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Why a worker ended or an attempt failed.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The account is live, but its mailbox cannot store flag colours.
    #[error(
        "account {account:?} would write flags, but its mailbox does not allow custom keywords \
         (PERMANENTFLAGS lacks \\*). A bare \\Flagged shows as red in Apple Mail, and red is yours. \
         Set `flagging = false` or `dry_run = true` for this account"
    )]
    LiveWithoutKeywords {
        /// The account's name in the config.
        account: String,
    },
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
            WorkerError::LiveWithoutKeywords { .. }
            | WorkerError::Config(_)
            | WorkerError::Store(_)
            | WorkerError::Task(_) => true,
        }
    }
}

/// The Sent-folder refresh inside the clear pass shares its error cases with the worker.
impl From<HarvestError> for WorkerError {
    fn from(error: HarvestError) -> WorkerError {
        match error {
            HarvestError::Imap(error) => WorkerError::Imap(error),
            HarvestError::Store(error) => WorkerError::Store(error),
            HarvestError::Task(error) => WorkerError::Task(error),
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

/// Splits owned flags into those that belong to the mailbox's current UIDVALIDITY and the
/// rest. A UID only means something together with its UIDVALIDITY, so the rest can never be
/// acted on.
fn split_by_uidvalidity(
    owned: Vec<OwnedFlag>,
    uid_validity: u32,
) -> (Vec<OwnedFlag>, Vec<OwnedFlag>) {
    owned
        .into_iter()
        .partition(|flag| flag.key.uidvalidity == uid_validity)
}

/// The guard for going live: an account that writes flags needs a mailbox that can store the
/// colour keywords. `Err` (fatal) when it does not.
///
/// Without custom keywords a STORE would add a bare `\Flagged`, which Apple Mail shows as red,
/// the user's own colour. Checked on every `SELECT`, not only in `check`, because the server's
/// answer can change and `check` may never have been run.
pub fn ensure_keywords(
    account: &str,
    mode: FlagMode,
    keywords_allowed: bool,
) -> Result<(), WorkerError> {
    if mode == FlagMode::Live && !keywords_allowed {
        return Err(WorkerError::LiveWithoutKeywords {
            account: account.to_owned(),
        });
    }
    Ok(())
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

/// What the flag logic needs from a mailbox: read one message's flags, change them.
///
/// A trait only so the unit tests can stand in a fake mailbox and prove what *would* be sent to
/// a server, without one. Production uses [`ImapSession`]. The methods return
/// `impl Future + Send` (as [`Decider`] does) so that the worker's future stays `Send` and can
/// run on tokio's thread pool.
trait FlagBackend {
    /// The flags the server reports for `uid` right now, or `None` if the message is gone.
    fn current_flags(
        &mut self,
        uid: u32,
    ) -> impl Future<Output = Result<Option<Vec<String>>, ImapError>> + Send;

    /// Applies a flag change to `uid`.
    fn change_flags(
        &mut self,
        uid: u32,
        change: &StoreChange,
    ) -> impl Future<Output = Result<(), ImapError>> + Send;
}

impl FlagBackend for ImapSession {
    async fn current_flags(&mut self, uid: u32) -> Result<Option<Vec<String>>, ImapError> {
        Ok(self
            .fetch_meta(&[uid])
            .await?
            .into_iter()
            .next()
            .map(|meta| meta.flags))
    }

    async fn change_flags(&mut self, uid: u32, change: &StoreChange) -> Result<(), ImapError> {
        self.store_flags(uid, change).await
    }
}

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

/// What [`Ctx::set_flag`] did about the flag a message was meant to get.
enum FlagOutcome {
    /// `choose_flag` found nothing to set (or the account does not flag).
    NotWanted,
    /// Dry-run: logged what would happen, changed nothing.
    WouldSet,
    /// The STORE succeeded. Carries the ownership record to save.
    Set(OwnedFlag),
    /// Left alone on purpose: the user flagged it first, or the colour was red.
    LeftAlone,
    /// The message vanished from the server before we could flag it.
    Gone,
}

/// What one run of the clear pass did.
#[derive(Debug, Default, PartialEq, Eq)]
struct ClearReport {
    /// Flags removed from the server.
    cleared: u32,
    /// Dry-run: flags that would have been removed.
    would_clear: u32,
    /// Messages whose flag the user changed; ours no more.
    user_owned: u32,
    /// Flagged messages that no longer exist.
    gone: u32,
}

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
        tracing::info!(
            account = %self.account.name, idle = use_idle, mode = self.mode().as_str(),
            "connected"
        );

        let inbox = self.account.inbox.as_str();
        let name = self.account.name.clone();
        let mailbox = inbox.to_owned();
        let mut saved = self
            .with_store(move |store| store.mailbox_state(&name, &mailbox))
            .await?;

        // When the clear pass last ran on this connection; `None` means "run it right away".
        let mut last_clear: Option<Instant> = None;

        loop {
            let info = session.select(inbox).await?;
            ensure_keywords(&self.account.name, self.mode(), info.keywords_allowed)?;

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

            if last_clear.is_none_or(|at| at.elapsed() >= self.timing.clear_interval) {
                if self
                    .clear_pass(&mut session, state.uidvalidity)
                    .await?
                    .is_break()
                {
                    return finish(session).await;
                }
                last_clear = Some(Instant::now());
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
        let provider = self.account.provider_headers;
        let parsed = match plan {
            FetchPlan::Full => parse_message(&raw, authserv, provider),
            FetchPlan::HeadersOnly => parse_headers_only(&raw, authserv, provider),
        };
        let Ok(parsed) = parsed else {
            tracing::warn!(account = %self.account.name, uid = meta.uid, "cannot parse the message; skipping");
            return Ok(Outcome::Skipped(SkipReason::Unparseable));
        };

        self.triage(session, key, &parsed, failed_attempts).await
    }

    /// The pipeline after parsing: own-address skip, rule and known-sender lookup,
    /// `screen_sender`, decider, `choose_flag`, then the flag itself, then the record. The
    /// mailbox is reached only through [`FlagBackend`], which is what lets the unit tests run
    /// it with a fake decider, a fake mailbox and an in-memory database.
    async fn triage<B: FlagBackend>(
        &self,
        backend: &mut B,
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

        // An account with `flagging = false` is classified for the digest only, so there is
        // no flag to choose, not even a would-be one.
        let flag = if self.account.flagging {
            choose_flag(trust, &verdict, &self.policy)
        } else {
            None
        };

        // The STORE comes first and the records after it. A STORE that fails (`?`) records
        // nothing, so after the reconnect the message is looked at again; a record is never
        // written for a flag that is not on the server.
        let outcome = match flag {
            Some(flag) => {
                self.set_flag(backend, key, flag, &verdict, parsed.message_id.as_deref())
                    .await?
            }
            None => FlagOutcome::NotWanted,
        };
        let (recorded_flag, ownership) = match outcome {
            FlagOutcome::Gone => return Ok(Outcome::Gone),
            // Dry-run keeps the would-be flag in the log tables, so `stats` can show it.
            FlagOutcome::WouldSet => (flag, None),
            FlagOutcome::Set(owned) => (flag, Some(owned)),
            FlagOutcome::NotWanted | FlagOutcome::LeftAlone => (None, None),
        };
        if flag.is_none() {
            tracing::info!(
                account = %self.account.name,
                uid = key.uid,
                category = verdict.category.as_str(),
                source,
                "{}",
                if self.account.flagging {
                    "classified: no flag needed"
                } else {
                    "classified for the digest (flagging is off for this account)"
                }
            );
        }

        let record = ProcessedRecord {
            verdict: Some(verdict.clone()),
            flag: recorded_flag,
            source: String::from(source),
            ..base
        };
        let label = LabelRecord {
            key: key.clone(),
            features: features_json(&evidence, trust, parsed.from_domain()),
            verdict,
            flag: recorded_flag,
        };
        self.with_store(move |store| {
            if let Some(owned) = &ownership {
                store.record_flag(owned)?;
            }
            store.record_processed(&record)?;
            store.record_label(&label)
        })
        .await?;
        Ok(Outcome::Processed)
    }

    /// Sets the chosen flag on the server, when the account is live.
    ///
    /// The flags are read again **right before** the STORE, not taken from the download a
    /// moment ago: the user may have flagged the message in between (mail takes seconds to
    /// classify with a local model). Any `\Flagged` found then means "the user got there
    /// first" and the message is left alone. See [`plan_apply`].
    async fn set_flag<B: FlagBackend>(
        &self,
        backend: &mut B,
        key: &MessageKey,
        flag: TriageFlag,
        verdict: &Verdict,
        message_id: Option<&str>,
    ) -> Result<FlagOutcome, WorkerError> {
        let colour = self.policy.colour_for(flag);
        let name = self.config.flag_name(flag);

        if self.mode() != FlagMode::Live {
            tracing::info!(
                account = %self.account.name, uid = key.uid,
                category = verdict.category.as_str(), flag = name, colour = ?colour,
                "dry-run: would set flag (nothing was changed on the server)"
            );
            return Ok(FlagOutcome::WouldSet);
        }

        let Some(current) = backend.current_flags(key.uid).await? else {
            return Ok(FlagOutcome::Gone);
        };
        match plan_apply(&current, colour) {
            ApplyPlan::Leave(LeaveReason::AlreadyFlagged) => {
                tracing::info!(
                    account = %self.account.name, uid = key.uid, wanted = name,
                    "the message is already flagged (by you); leaving it alone"
                );
                Ok(FlagOutcome::LeftAlone)
            }
            ApplyPlan::Leave(LeaveReason::ReservedRed) => {
                tracing::error!(
                    account = %self.account.name, uid = key.uid, wanted = name,
                    "refusing to set red, which is reserved for you; check [flags] in the config"
                );
                Ok(FlagOutcome::LeftAlone)
            }
            ApplyPlan::Store(change) => {
                backend.change_flags(key.uid, &change).await?;
                tracing::info!(
                    account = %self.account.name, uid = key.uid,
                    category = verdict.category.as_str(), flag = name, colour = ?colour,
                    "set flag"
                );
                Ok(FlagOutcome::Set(OwnedFlag {
                    key: key.clone(),
                    message_id: message_id.and_then(normalise_message_id),
                    colour,
                    flag: Some(flag),
                    deadline: (flag == TriageFlag::Deadline)
                        .then_some(verdict.deadline)
                        .flatten(),
                    reason: Some(verdict.reason.clone()),
                    set_at: now_unix(),
                }))
            }
        }
    }

    // ---- the auto-clear pass ----------------------------------------------------------

    /// Looks at every flag we set in this inbox and, where it is due, clears it.
    ///
    /// Steps: drop records of an old UIDVALIDITY (their UIDs mean nothing now, so nothing is
    /// ever stored for them); refresh the Sent harvest if a "Reply needed" flag is waiting for
    /// its reply; read the current flags of all owned messages; then [`Ctx::review_owned`].
    async fn clear_pass(
        &self,
        session: &mut ImapSession,
        uid_validity: u32,
    ) -> Result<Flow, WorkerError> {
        // `flagging = false` never wrote a flag, so there is nothing of ours to look after.
        if !self.account.flagging {
            return Ok(ControlFlow::Continue(()));
        }
        let (name, mailbox) = (self.account.name.clone(), self.account.inbox.clone());
        let owned = self
            .with_store(move |store| store.owned_flags(&name, &mailbox))
            .await?;
        if owned.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }

        // `set_mailbox_state` already deletes these when it sees the new UIDVALIDITY; this is
        // the second line of defence. Never a STORE for a UID of an older numbering: it could
        // hit a different message.
        let (current, stale) = split_by_uidvalidity(owned, uid_validity);
        for old in stale {
            tracing::warn!(
                account = %self.account.name, uid = old.key.uid,
                "forgetting a flag record from before the UIDVALIDITY change; nothing is stored"
            );
            let key = old.key;
            self.with_store(move |store| store.clear_flag_record(&key))
                .await?;
        }
        if current.is_empty() {
            return Ok(ControlFlow::Continue(()));
        }

        // A reply shows up in Sent, not in the inbox, so bring the reply ids up to date. The
        // harvest reads only new Sent messages and leaves Sent selected; select the inbox
        // again afterwards because everything below, and the IDLE that follows, needs it.
        if current
            .iter()
            .any(|flag| flag.flag == Some(TriageFlag::ReplyNeeded))
        {
            harvest_known_senders(
                session,
                self.store,
                self.account,
                self.config.policy.known_senders_from_sent,
            )
            .await?;
            let reselected = session.select(&self.account.inbox).await?;
            if reselected.uid_validity != uid_validity {
                // Renumbered while we were looking at Sent. The main loop handles that.
                return Ok(ControlFlow::Continue(()));
            }
        }

        let uids: Vec<u32> = current.iter().map(|flag| flag.key.uid).collect();
        let mut server_flags: HashMap<u32, Vec<String>> = HashMap::new();
        for chunk in uids.chunks(META_BATCH as usize) {
            for meta in session.fetch_meta(chunk).await? {
                server_flags.insert(meta.uid, meta.flags);
            }
        }

        let today = jiff::Zoned::now().date();
        let report = self
            .review_owned(session, current, &server_flags, today)
            .await?;
        if report != ClearReport::default() {
            tracing::info!(
                account = %self.account.name, cleared = report.cleared,
                would_clear = report.would_clear, taken_over_by_user = report.user_owned,
                gone = report.gone, "flag review finished"
            );
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Decides and acts for each flag we own, given what the server reports for them.
    ///
    /// For every record:
    ///
    /// 1. Message gone (deleted or moved by the user): forget the record, STORE nothing.
    /// 2. Flags differ from what we set: the user took over. Mark it user-owned; never again.
    /// 3. Still ours: if [`clear_reason`] says so, re-read the flags once more, then one STORE
    ///    removing `\Flagged` and our bits, then forget the record.
    ///
    /// In dry-run the same decisions are only logged ("would clear"), and neither the server
    /// nor the database changes.
    async fn review_owned<B: FlagBackend>(
        &self,
        backend: &mut B,
        owned: Vec<OwnedFlag>,
        server_flags: &HashMap<u32, Vec<String>>,
        today: Date,
    ) -> Result<ClearReport, WorkerError> {
        let live = self.mode() == FlagMode::Live;
        let mut report = ClearReport::default();

        for record in owned {
            if self.shutdown.is_cancelled() {
                break;
            }
            let uid = record.key.uid;
            let account = self.account.name.as_str();

            let Some(flags) = server_flags.get(&uid) else {
                report.gone += 1;
                tracing::info!(
                    account,
                    uid,
                    "a flagged message is gone from the inbox; forgetting it"
                );
                if live {
                    let key = record.key.clone();
                    self.with_store(move |store| store.clear_flag_record(&key))
                        .await?;
                }
                continue;
            };

            if owned_state(flags, record.colour) == OwnedState::ChangedByUser {
                report.user_owned += 1;
                tracing::info!(
                    account,
                    uid,
                    "the flag was changed by you; leaving this message alone from now on"
                );
                if live {
                    let key = record.key.clone();
                    self.with_store(move |store| store.mark_user_owned(&key).map(|_| ()))
                        .await?;
                }
                continue;
            }

            // Has the user answered? Only "Reply needed" asks, and only when the message has
            // an id to match against the Sent folder.
            let replied = match (record.flag, record.message_id.clone()) {
                (Some(TriageFlag::ReplyNeeded), Some(id)) => {
                    let name = self.account.name.clone();
                    self.with_store(move |store| store.has_reply_to(&name, &id))
                        .await?
                }
                _ => false,
            };
            let Some(reason) = clear_reason(record.flag, record.deadline, replied, today) else {
                continue;
            };

            if !live {
                report.would_clear += 1;
                tracing::info!(
                    account,
                    uid,
                    reason = reason.as_str(),
                    "dry-run: would clear flag (nothing was changed on the server)"
                );
                continue;
            }

            // Look once more right before writing: the user may have just changed the flag.
            let Some(fresh) = backend.current_flags(uid).await? else {
                continue; // gone since the batch read; the next pass records that
            };
            if owned_state(&fresh, record.colour) == OwnedState::ChangedByUser {
                // Leave it for the next pass, which marks it user-owned in the normal way.
                continue;
            }
            backend.change_flags(uid, &clear_change()).await?;
            let key = record.key.clone();
            self.with_store(move |store| store.clear_flag_record(&key))
                .await?;
            report.cleared += 1;
            tracing::info!(account, uid, reason = reason.as_str(), "cleared flag");
        }
        Ok(report)
    }

    /// What this account may do to the server (see [`AccountConfig::flag_mode`]).
    fn mode(&self) -> FlagMode {
        self.account.flag_mode(&self.config.policy)
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

// ---------------------------------------------------------------------------------------------
// Backfill
// ---------------------------------------------------------------------------------------------

/// What [`backfill_account`] found and did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    /// Messages in the inbox that arrived within the window.
    pub found: usize,
    /// Classified (and flagged, on a live account) now.
    pub processed: usize,
    /// Left alone: already processed, flagged by the user, the account's own mail, unreadable.
    pub skipped: usize,
    /// The model failed; the message is in the retry queue and the running service retries it.
    pub deferred: usize,
    /// Messages that disappeared while the backfill ran.
    pub gone: usize,
    /// True if shutdown (Ctrl-C) stopped the run before it finished.
    pub interrupted: bool,
}

/// Runs the normal pipeline over inbox mail that arrived since `since`, oldest first.
///
/// It is the same [`Ctx::process_message`] the live worker uses, so everything holds here too:
/// already processed mail is skipped, the account's dry-run setting decides whether anything
/// is written, flags are re-read before a STORE, and the model's concurrency limit applies
/// (the shared decider holds the permits; messages are handled one at a time anyway).
///
/// **It never touches the worker's saved position** (`mailbox_state`). The live worker's
/// cursor only moves forward through new mail, and a backfill that wrote it could move it
/// backwards and make the worker re-read old mail, or forwards and make it skip new mail.
/// Messages the backfill has handled are in the `processed` table, which is all the worker
/// needs to skip them later.
///
/// Opens its own connection, so it can run next to a running service.
pub async fn backfill_account<D: Decider>(
    account: &AccountConfig,
    config: &Config,
    decider: Arc<D>,
    store: Arc<Mutex<Store>>,
    shutdown: CancellationToken,
    since: Date,
) -> Result<BackfillReport, WorkerError> {
    let for_password = account.clone();
    let password = tokio::task::spawn_blocking(move || for_password.password()).await??;
    let settings = account.imap_settings(&password);
    let timing = WorkerTiming::default();
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

    let mut session = ImapSession::connect(&settings).await?;
    let info = session.select(&account.inbox).await?;
    ensure_keywords(&account.name, ctx.mode(), info.keywords_allowed)?;

    let uids = session.search_since(since).await?;
    let mut report = BackfillReport {
        found: uids.len(),
        ..BackfillReport::default()
    };
    tracing::info!(
        account = %account.name, since = %since, messages = uids.len(), mode = ctx.mode().as_str(),
        "backfill starting"
    );

    'batches: for chunk in uids.chunks(META_BATCH as usize) {
        // The server may answer in any order; oldest first is what we promised.
        let mut metas = session.fetch_meta(chunk).await?;
        metas.sort_by_key(|meta| meta.uid);
        for meta in metas {
            if shutdown.is_cancelled() {
                report.interrupted = true;
                break 'batches;
            }
            let key = ctx.key(info.uid_validity, meta.uid);
            match ctx.process_message(&mut session, &meta, &key, 0).await? {
                Outcome::Processed => report.processed += 1,
                Outcome::Skipped(_) => report.skipped += 1,
                Outcome::Deferred => report.deferred += 1,
                Outcome::Gone => report.gone += 1,
                Outcome::Cancelled => {
                    report.interrupted = true;
                    break 'batches;
                }
            }
        }
    }
    let _ = session.logout().await;
    Ok(report)
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
    use crate::flags::FlagColour;
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
        assert!(
            WorkerError::LiveWithoutKeywords {
                account: "a".into()
            }
            .is_fatal()
        );
    }

    #[test]
    fn going_live_needs_custom_keywords() {
        // Live without keywords: refused, with a message that says what to do.
        let error = ensure_keywords("zoho", FlagMode::Live, false).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("zoho"), "{text}");
        assert!(text.contains("custom keywords"), "{text}");
        assert!(text.contains("flagging = false"), "{text}");
        assert!(text.contains("dry_run = true"), "{text}");
        // Everything else is fine: keywords present, or nothing is written anyway.
        assert!(ensure_keywords("zoho", FlagMode::Live, true).is_ok());
        assert!(ensure_keywords("wp", FlagMode::DryRun, false).is_ok());
        assert!(ensure_keywords("wp", FlagMode::ClassifyOnly, false).is_ok());
    }

    /// Hard rule: the worker may change flags (through `store_flags`, which checks for custom
    /// keywords) and nothing else. No raw `STORE`, and nothing that appends, moves, copies,
    /// expunges or deletes mail. The forbidden words are assembled at runtime so this test does
    /// not match itself.
    #[test]
    fn worker_source_only_changes_flags() {
        let production_code = include_str!("worker.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in [
            ["uid", "_store"].concat(),
            [".app", "end("].concat(),
            [".exp", "unge("].concat(),
            [".uid", "_expunge("].concat(),
            [".uid", "_mv("].concat(),
            [".mv", "("].concat(),
            [".uid", "_copy("].concat(),
            [".copy", "("].concat(),
            [".del", "ete("].concat(),
            ["\\Del", "eted"].concat(),
        ] {
            assert!(
                !production_code.contains(&forbidden),
                "the worker must not use {forbidden}"
            );
        }
    }

    #[test]
    fn flags_for_an_older_uidvalidity_are_set_apart() {
        let flag = |uidvalidity| OwnedFlag {
            key: MessageKey {
                uidvalidity,
                ..key(1)
            },
            message_id: None,
            colour: crate::flags::FlagColour::Orange,
            flag: Some(TriageFlag::ReplyNeeded),
            deadline: None,
            reason: None,
            set_at: 0,
        };
        let (current, stale) = split_by_uidvalidity(vec![flag(1), flag(2), flag(1)], 1);
        assert_eq!((current.len(), stale.len()), (2, 1));
        assert_eq!(stale[0].key.uidvalidity, 2);
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

    // ---- a fake mailbox ----------------------------------------------------------------

    /// A stand-in for the server: the flags of each UID, plus a log of every change sent to it.
    /// Changes are applied (remove, then add) so tests can check the end state too.
    #[derive(Default)]
    struct FakeMailbox {
        flags: HashMap<u32, Vec<String>>,
        changes: Vec<(u32, StoreChange)>,
        fail_changes: bool,
    }

    impl FakeMailbox {
        fn with_message(uid: u32, flags: &[&str]) -> FakeMailbox {
            let mut mailbox = FakeMailbox::default();
            mailbox.flags.insert(uid, flag_list(flags));
            mailbox
        }
    }

    impl FlagBackend for FakeMailbox {
        async fn current_flags(&mut self, uid: u32) -> Result<Option<Vec<String>>, ImapError> {
            Ok(self.flags.get(&uid).cloned())
        }

        async fn change_flags(&mut self, uid: u32, change: &StoreChange) -> Result<(), ImapError> {
            if self.fail_changes {
                return Err(ImapError::Timeout);
            }
            self.changes.push((uid, change.clone()));
            let flags = self.flags.entry(uid).or_default();
            flags.retain(|f| !change.remove.iter().any(|r| r.eq_ignore_ascii_case(f)));
            for add in &change.add {
                if !flags.iter().any(|f| f.eq_ignore_ascii_case(add)) {
                    flags.push((*add).to_owned());
                }
            }
            Ok(())
        }
    }

    fn flag_list(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    /// Everything a `Ctx` borrows, owned in one place, so a test can build a context in a line.
    struct Fixture {
        account: AccountConfig,
        config: Config,
        store: Arc<Mutex<Store>>,
        shutdown: CancellationToken,
        timing: WorkerTiming,
    }

    impl Fixture {
        /// `extra` is added to the account's TOML (for example `dry_run = false`). The policy
        /// stays at its default (`dry_run = true`), so a test that wants a live account must
        /// say so on the account, which is also the intended way to use the setting.
        fn new(extra: &str) -> Fixture {
            let account = toml::from_str(&format!(
                "name = \"test\"\nhost = \"127.0.0.1\"\nusername = \"me@example.org\"\n{extra}"
            ))
            .unwrap();
            Fixture {
                account,
                config: Config::default(),
                store: Arc::new(Mutex::new(Store::open_in_memory().unwrap())),
                shutdown: CancellationToken::new(),
                timing: WorkerTiming::default(),
            }
        }

        /// An account that writes flags.
        fn live() -> Fixture {
            Fixture::new("dry_run = false\n")
        }

        fn ctx<'a, D: Decider>(&'a self, decider: &'a D) -> Ctx<'a, D> {
            Ctx {
                account: &self.account,
                config: &self.config,
                policy: self.config.flag_policy(),
                decider,
                store: &self.store,
                shutdown: &self.shutdown,
                timing: &self.timing,
                own: own_address(&self.account),
            }
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
        // Dry-run: the mailbox must never be asked to change anything.
        let mut mailbox = FakeMailbox::default();
        let outcome = ctx
            .triage(&mut mailbox, &key(5), &parsed(from), 0)
            .await
            .unwrap();
        assert!(mailbox.changes.is_empty(), "dry-run changed the mailbox");
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
    async fn an_account_with_flagging_off_is_classified_without_a_flag() {
        let decider = FakeDecider::answering(reply_needed());
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        // Struct-update syntax (`..account(...)`) is not allowed here: `AccountConfig` has a
        // private field (`port`), so we change the one field on a mutable copy instead.
        let mut account = account("me@example.org");
        account.flagging = false;
        let config = Config::default();
        let (shutdown, timing) = (CancellationToken::new(), WorkerTiming::default());
        let ctx = Ctx {
            account: &account,
            config: &config,
            policy: config.flag_policy(),
            decider: &decider,
            store: &store,
            shutdown: &shutdown,
            timing: &timing,
            own: own_address(&account),
        };
        let mut mailbox = FakeMailbox::with_message(5, &[]);
        let outcome = ctx
            .triage(&mut mailbox, &key(5), &parsed("anna@friend.example"), 0)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Processed);
        assert!(mailbox.changes.is_empty());
        let store = store.lock().unwrap();
        // Classified and recorded for the digest, but no flag, not even a would-be one.
        assert!(store.is_processed(&key(5)).unwrap());
        assert!(store.flag_counts(0).unwrap().is_empty());
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

    // ---- setting flags (task 4.1) --------------------------------------------------------

    async fn triage_live(
        fx: &Fixture,
        mailbox: &mut FakeMailbox,
        verdict: Verdict,
    ) -> Result<Outcome, WorkerError> {
        let decider = FakeDecider::answering(verdict);
        fx.ctx(&decider)
            .triage(mailbox, &key(5), &parsed("anna@friend.example"), 0)
            .await
    }

    #[tokio::test]
    async fn a_live_account_stores_the_flag_and_records_ownership() {
        // The policy says dry-run; the account says live. The account wins.
        let fx = Fixture::live();
        assert!(fx.config.policy.dry_run);
        let mut mailbox = FakeMailbox::with_message(5, &["\\Seen"]);

        let outcome = triage_live(&fx, &mut mailbox, reply_needed())
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Processed);
        assert_eq!(mailbox.changes.len(), 1, "exactly one change is sent");
        let (uid, change) = &mailbox.changes[0];
        assert_eq!(*uid, 5);
        assert_eq!(change.add, ["\\Flagged", "$MailFlagBit0"]);
        assert_eq!(change.remove, ["$MailFlagBit1", "$MailFlagBit2"]);
        // The end state on the "server" is orange, and \Seen is still there.
        let on_server = &mailbox.flags[&5];
        let strs: Vec<&str> = on_server.iter().map(String::as_str).collect();
        assert_eq!(FlagColour::from_flags(&strs), Some(FlagColour::Orange));
        assert!(on_server.iter().any(|f| f == "\\Seen"));

        let store = fx.store.lock().unwrap();
        let owned = store.owned_flags("test", "INBOX").unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].key, key(5));
        assert_eq!(owned[0].colour, FlagColour::Orange);
        assert_eq!(owned[0].flag, Some(TriageFlag::ReplyNeeded));
        // The Message-ID is stored in matching form, for the reply check later.
        assert_eq!(owned[0].message_id.as_deref(), Some("1@example.org"));
        assert_eq!(store.flag_counts(0).unwrap()[0].label, "reply_needed");
    }

    #[tokio::test]
    async fn a_deadline_flag_remembers_its_date() {
        let mut fx = Fixture::live();
        fx.config.flags.deadline.enabled = Some(true);
        let verdict = Verdict {
            needs_reply: false,
            deadline: Some(jiff::civil::date(2026, 11, 30)),
            ..reply_needed()
        };
        let mut mailbox = FakeMailbox::with_message(5, &[]);
        triage_live(&fx, &mut mailbox, verdict).await.unwrap();

        let owned = fx
            .store
            .lock()
            .unwrap()
            .owned_flags("test", "INBOX")
            .unwrap();
        assert_eq!(owned[0].flag, Some(TriageFlag::Deadline));
        assert_eq!(owned[0].colour, FlagColour::Yellow);
        assert_eq!(owned[0].deadline, Some(jiff::civil::date(2026, 11, 30)));
    }

    #[tokio::test]
    async fn a_dry_run_account_stores_nothing_and_records_no_ownership() {
        let fx = Fixture::new("dry_run = true\n");
        let mut mailbox = FakeMailbox::with_message(5, &[]);
        let outcome = triage_live(&fx, &mut mailbox, reply_needed())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Processed);
        assert!(mailbox.changes.is_empty());
        let store = fx.store.lock().unwrap();
        assert!(store.owned_flags("test", "INBOX").unwrap().is_empty());
        // The would-be flag is still logged for `stats`.
        assert_eq!(store.flag_counts(0).unwrap()[0].label, "reply_needed");
    }

    #[tokio::test]
    async fn an_account_with_flagging_off_never_stores_even_when_live() {
        let fx = Fixture::new("dry_run = false\nflagging = false\n");
        let mut mailbox = FakeMailbox::with_message(5, &[]);
        let outcome = triage_live(&fx, &mut mailbox, reply_needed())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Processed);
        assert!(mailbox.changes.is_empty());
        let store = fx.store.lock().unwrap();
        assert!(store.owned_flags("test", "INBOX").unwrap().is_empty());
        assert!(store.flag_counts(0).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_message_the_user_flagged_in_the_meantime_is_left_alone() {
        // The download showed no flag; by the time we write, the user has flagged it red.
        let fx = Fixture::live();
        let mut mailbox = FakeMailbox::with_message(5, &["\\Flagged"]);

        let outcome = triage_live(&fx, &mut mailbox, reply_needed())
            .await
            .unwrap();

        assert_eq!(
            outcome,
            Outcome::Processed,
            "still classified for the digest"
        );
        assert!(mailbox.changes.is_empty(), "the user's flag was touched");
        let store = fx.store.lock().unwrap();
        assert!(store.owned_flags("test", "INBOX").unwrap().is_empty());
        assert!(store.flag_counts(0).unwrap().is_empty(), "we set no flag");
        assert!(store.is_processed(&key(5)).unwrap());
    }

    #[tokio::test]
    async fn red_is_never_stored_even_if_the_policy_asks_for_it() {
        let fx = Fixture::live();
        let decider = FakeDecider::answering(reply_needed());
        let mut ctx = fx.ctx(&decider);
        // The config loader rejects red, so reach past it: a second line of defence.
        ctx.policy.colours.reply_needed = FlagColour::Red;
        let mut mailbox = FakeMailbox::with_message(5, &[]);

        let outcome = ctx
            .triage(&mut mailbox, &key(5), &parsed("anna@friend.example"), 0)
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Processed);
        assert!(mailbox.changes.is_empty());
        assert!(
            fx.store
                .lock()
                .unwrap()
                .owned_flags("test", "INBOX")
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_failed_store_records_nothing_so_the_message_is_retried() {
        let fx = Fixture::live();
        let mut mailbox = FakeMailbox::with_message(5, &[]);
        mailbox.fail_changes = true;

        let result = triage_live(&fx, &mut mailbox, reply_needed()).await;

        assert!(matches!(result, Err(WorkerError::Imap(_))), "{result:?}");
        let store = fx.store.lock().unwrap();
        assert!(store.owned_flags("test", "INBOX").unwrap().is_empty());
        assert!(!store.is_processed(&key(5)).unwrap(), "must be tried again");
    }

    #[tokio::test]
    async fn a_message_that_vanished_is_not_flagged_or_recorded() {
        let fx = Fixture::live();
        let mut mailbox = FakeMailbox::default(); // UID 5 does not exist
        let outcome = triage_live(&fx, &mut mailbox, reply_needed())
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Gone);
        assert!(mailbox.changes.is_empty());
        assert!(!fx.store.lock().unwrap().is_processed(&key(5)).unwrap());
    }

    // ---- the auto-clear pass (task 4.2) ---------------------------------------------------

    const TODAY: Date = jiff::civil::date(2026, 11, 10);

    /// A "Reply needed" record for UID `uid`, as `set_flag` would have written it.
    fn owned_reply(uid: u32) -> OwnedFlag {
        OwnedFlag {
            key: key(uid),
            message_id: Some("1@example.org".into()),
            colour: FlagColour::Orange,
            flag: Some(TriageFlag::ReplyNeeded),
            deadline: None,
            reason: Some("asks".into()),
            set_at: 100,
        }
    }

    fn owned_deadline(uid: u32, date: Date) -> OwnedFlag {
        OwnedFlag {
            colour: FlagColour::Yellow,
            flag: Some(TriageFlag::Deadline),
            deadline: Some(date),
            message_id: None,
            ..owned_reply(uid)
        }
    }

    const ORANGE: [&str; 3] = ["\\Seen", "\\Flagged", "$MailFlagBit0"];

    /// Runs the review for `records` against a mailbox that shows `server` for UID 5.
    async fn review(
        fx: &Fixture,
        records: Vec<OwnedFlag>,
        server: &[&str],
        today: Date,
    ) -> (ClearReport, FakeMailbox) {
        {
            let store = fx.store.lock().unwrap();
            for record in &records {
                store.record_flag(record).unwrap();
            }
        }
        let mut mailbox = FakeMailbox::with_message(5, server);
        let seen: HashMap<u32, Vec<String>> = mailbox.flags.clone();
        let decider = FakeDecider::failing();
        let report = fx
            .ctx(&decider)
            .review_owned(&mut mailbox, records, &seen, today)
            .await
            .unwrap();
        (report, mailbox)
    }

    fn owned_now(fx: &Fixture) -> Vec<OwnedFlag> {
        fx.store
            .lock()
            .unwrap()
            .owned_flags("test", "INBOX")
            .unwrap()
    }

    #[tokio::test]
    async fn a_reply_in_sent_clears_reply_needed() {
        let fx = Fixture::live();
        // The user's reply, as the Sent harvest would have stored it.
        fx.store
            .lock()
            .unwrap()
            .add_reply_ids("test", &["1@example.org".into()])
            .unwrap();

        let (report, mailbox) = review(&fx, vec![owned_reply(5)], &ORANGE, TODAY).await;

        assert_eq!(report.cleared, 1);
        // One change: remove \Flagged and all three bits. Nothing is added.
        assert_eq!(mailbox.changes, [(5, clear_change())]);
        // \Seen survives; the colour is gone; the record is forgotten.
        assert_eq!(mailbox.flags[&5], ["\\Seen"]);
        assert!(owned_now(&fx).is_empty());
    }

    #[tokio::test]
    async fn without_a_reply_the_flag_stays() {
        let fx = Fixture::live();
        let (report, mailbox) = review(&fx, vec![owned_reply(5)], &ORANGE, TODAY).await;
        assert_eq!(report, ClearReport::default());
        assert!(mailbox.changes.is_empty());
        assert_eq!(owned_now(&fx).len(), 1);
    }

    #[tokio::test]
    async fn a_reply_to_another_message_does_not_clear_it() {
        let fx = Fixture::live();
        fx.store
            .lock()
            .unwrap()
            .add_reply_ids("test", &["someone-else@example.org".into()])
            .unwrap();
        let (_, mailbox) = review(&fx, vec![owned_reply(5)], &ORANGE, TODAY).await;
        assert!(mailbox.changes.is_empty());
    }

    #[tokio::test]
    async fn a_colour_the_user_changed_becomes_user_owned_and_is_never_touched() {
        let fx = Fixture::live();
        fx.store
            .lock()
            .unwrap()
            .add_reply_ids("test", &["1@example.org".into()])
            .unwrap();
        // Green instead of the orange we set. Even with the reply present, hands off.
        let green = ["\\Flagged", "$MailFlagBit0", "$MailFlagBit1"];

        let (report, mailbox) = review(&fx, vec![owned_reply(5)], &green, TODAY).await;

        assert_eq!(report.user_owned, 1);
        assert!(mailbox.changes.is_empty());
        let store = fx.store.lock().unwrap();
        assert!(store.is_user_owned(&key(5)).unwrap());
        assert!(store.owned_flags("test", "INBOX").unwrap().is_empty());
        drop(store);

        // And it stays that way: a later pass has nothing to look at, and recording a new
        // flag for the message cannot take it back.
        fx.store
            .lock()
            .unwrap()
            .record_flag(&owned_reply(5))
            .unwrap();
        assert!(owned_now(&fx).is_empty());
    }

    #[tokio::test]
    async fn a_flag_the_user_removed_becomes_user_owned() {
        let fx = Fixture::live();
        let (report, mailbox) = review(&fx, vec![owned_reply(5)], &["\\Seen"], TODAY).await;
        assert_eq!(report.user_owned, 1);
        assert!(mailbox.changes.is_empty());
        assert!(fx.store.lock().unwrap().is_user_owned(&key(5)).unwrap());
    }

    #[tokio::test]
    async fn a_red_flag_means_the_user_took_over() {
        let fx = Fixture::live();
        let (report, mailbox) = review(&fx, vec![owned_reply(5)], &["\\Flagged"], TODAY).await;
        assert_eq!(report.user_owned, 1);
        assert!(mailbox.changes.is_empty());
    }

    #[tokio::test]
    async fn a_deadline_flag_clears_after_its_date_and_not_before() {
        let yellow = ["\\Flagged", "$MailFlagBit1"];

        let fx = Fixture::live();
        let record = owned_deadline(5, jiff::civil::date(2026, 11, 9));
        let (report, mailbox) = review(&fx, vec![record], &yellow, TODAY).await;
        assert_eq!(report.cleared, 1);
        assert_eq!(mailbox.changes, [(5, clear_change())]);

        // On the deadline day itself the flag stays.
        let fx = Fixture::live();
        let record = owned_deadline(5, jiff::civil::date(2026, 11, 10));
        let (report, mailbox) = review(&fx, vec![record], &yellow, TODAY).await;
        assert_eq!(report, ClearReport::default());
        assert!(mailbox.changes.is_empty());
    }

    #[tokio::test]
    async fn a_deadline_flag_without_a_date_is_never_cleared() {
        let fx = Fixture::live();
        let record = OwnedFlag {
            deadline: None,
            ..owned_deadline(5, jiff::civil::date(2020, 1, 1))
        };
        let (_, mailbox) = review(&fx, vec![record], &["\\Flagged", "$MailFlagBit1"], TODAY).await;
        assert!(mailbox.changes.is_empty());
    }

    #[tokio::test]
    async fn flags_that_wait_for_the_user_are_never_cleared() {
        let fx = Fixture::live();
        fx.store
            .lock()
            .unwrap()
            .add_reply_ids("test", &["1@example.org".into()])
            .unwrap();
        let record = OwnedFlag {
            colour: FlagColour::Green,
            flag: Some(TriageFlag::ToPay),
            deadline: Some(jiff::civil::date(2020, 1, 1)),
            ..owned_reply(5)
        };
        let green = ["\\Flagged", "$MailFlagBit0", "$MailFlagBit1"];
        let (_, mailbox) = review(&fx, vec![record], &green, TODAY).await;
        assert!(mailbox.changes.is_empty());
        assert_eq!(owned_now(&fx).len(), 1);
    }

    #[tokio::test]
    async fn a_dry_run_account_only_says_what_it_would_clear() {
        let fx = Fixture::new("dry_run = true\n");
        fx.store
            .lock()
            .unwrap()
            .add_reply_ids("test", &["1@example.org".into()])
            .unwrap();
        let (report, mailbox) = review(&fx, vec![owned_reply(5)], &ORANGE, TODAY).await;
        assert_eq!(report.would_clear, 1);
        assert_eq!(report.cleared, 0);
        assert!(mailbox.changes.is_empty(), "dry-run changed the mailbox");
        // Neither does the database change.
        assert_eq!(owned_now(&fx).len(), 1);
    }

    #[tokio::test]
    async fn a_dry_run_account_does_not_even_mark_user_changes() {
        let fx = Fixture::new("dry_run = true\n");
        let (report, _) = review(&fx, vec![owned_reply(5)], &["\\Seen"], TODAY).await;
        assert_eq!(report.user_owned, 1, "reported");
        assert!(!fx.store.lock().unwrap().is_user_owned(&key(5)).unwrap());
    }

    #[tokio::test]
    async fn a_flagged_message_that_is_gone_is_forgotten_without_a_store() {
        let fx = Fixture::live();
        fx.store
            .lock()
            .unwrap()
            .record_flag(&owned_reply(7))
            .unwrap();
        let mut mailbox = FakeMailbox::default();
        let decider = FakeDecider::failing();
        let report = fx
            .ctx(&decider)
            .review_owned(&mut mailbox, vec![owned_reply(7)], &HashMap::new(), TODAY)
            .await
            .unwrap();
        assert_eq!(report.gone, 1);
        assert!(mailbox.changes.is_empty());
        assert!(owned_now(&fx).is_empty());
    }

    #[tokio::test]
    async fn a_flag_changed_just_before_the_store_is_not_cleared() {
        // The batch read showed our orange flag; the re-read right before the STORE shows
        // that the user has since turned it green.
        let fx = Fixture::live();
        fx.store
            .lock()
            .unwrap()
            .add_reply_ids("test", &["1@example.org".into()])
            .unwrap();
        fx.store
            .lock()
            .unwrap()
            .record_flag(&owned_reply(5))
            .unwrap();
        let batch: HashMap<u32, Vec<String>> = HashMap::from([(5, flag_list(&ORANGE))]);
        let mut mailbox =
            FakeMailbox::with_message(5, &["\\Flagged", "$MailFlagBit0", "$MailFlagBit1"]);
        let decider = FakeDecider::failing();

        let report = fx
            .ctx(&decider)
            .review_owned(&mut mailbox, vec![owned_reply(5)], &batch, TODAY)
            .await
            .unwrap();

        assert_eq!(report.cleared, 0);
        assert!(mailbox.changes.is_empty());
    }

    #[tokio::test]
    async fn a_failed_clear_keeps_the_record_for_the_next_pass() {
        let fx = Fixture::live();
        fx.store
            .lock()
            .unwrap()
            .add_reply_ids("test", &["1@example.org".into()])
            .unwrap();
        fx.store
            .lock()
            .unwrap()
            .record_flag(&owned_reply(5))
            .unwrap();
        let batch: HashMap<u32, Vec<String>> = HashMap::from([(5, flag_list(&ORANGE))]);
        let mut mailbox = FakeMailbox::with_message(5, &ORANGE);
        mailbox.fail_changes = true;
        let decider = FakeDecider::failing();

        let result = fx
            .ctx(&decider)
            .review_owned(&mut mailbox, vec![owned_reply(5)], &batch, TODAY)
            .await;

        assert!(result.is_err());
        assert_eq!(
            owned_now(&fx).len(),
            1,
            "the record must survive a failed STORE"
        );
    }
}
