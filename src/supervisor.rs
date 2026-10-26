//! The supervisor: runs every account's worker side by side and keeps them alive.
//!
//! Layers, from the inside out:
//!
//! - [`restart_decision`] is a pure function: given how a worker ended, should it be started
//!   again, and after how long?
//! - [`supervise`] runs one worker future in a loop and applies that decision. It is generic
//!   over a closure that *makes* the worker future, so the tests drive it with a fake worker
//!   and need no IMAP server.
//! - [`serve`] starts one task per account in a [`JoinSet`], waits for them, and enforces the
//!   shutdown grace period. It is also generic over the per-account future.
//! - [`run_service`] plugs in the real parts: [`run_account`] for the inbox and a periodic
//!   Sent-folder harvest on its own connection.
//!
//! # Why one task per account, and what `JoinSet` is for
//!
//! `tokio::spawn` starts a task and returns a handle you can await. A [`JoinSet`] is a bag of
//! such tasks: `join_next` hands back whichever finishes first, and `abort_all` cancels the
//! rest. Dropping a plain `JoinHandle` would *detach* the task (it keeps running unseen); a
//! `JoinSet` aborts its tasks when it is dropped, so nothing is left running by accident.
//!
//! # Shutdown
//!
//! One [`CancellationToken`] is shared by everything. When it fires, workers finish their
//! current step and return `Ok`. Anything still running after the grace period is aborted:
//! abort drops the future at its next `.await`, which is safe here because the database is
//! transactional and a message that was not recorded is simply classified again next time.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::{Instant, sleep};
use tokio_util::sync::CancellationToken;

use crate::config::{AccountConfig, Config};
use crate::decider::Decider;
use crate::imap::worker::{WorkerError, backoff_delay, run_account};
use crate::imap::{ImapError, ImapSession};
use crate::senders::harvest_known_senders;
use crate::store::Store;

// ---------------------------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------------------------

/// Every interval the supervisor uses. Tests shorten them; production uses [`Default`].
#[derive(Debug, Clone)]
pub struct SupervisorTiming {
    /// First wait before restarting a worker that ended with a fatal error; doubles each time.
    pub restart_initial: Duration,
    /// Ceiling for the restart wait.
    pub restart_max: Duration,
    /// How long workers get to stop after shutdown before they are aborted.
    pub grace: Duration,
    /// How often the Sent folder is harvested after a success.
    pub harvest_interval: Duration,
    /// How long to wait before harvesting again after a failure.
    pub harvest_retry: Duration,
}

impl Default for SupervisorTiming {
    fn default() -> Self {
        SupervisorTiming {
            restart_initial: Duration::from_secs(5 * 60),
            restart_max: Duration::from_secs(60 * 60),
            grace: Duration::from_secs(10),
            // Sent mail changes slowly and the harvest only reads new messages, so a few
            // times a day is plenty. A fixed interval is simpler than reacting to events.
            harvest_interval: Duration::from_secs(6 * 60 * 60),
            harvest_retry: Duration::from_secs(15 * 60),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The restart decision
// ---------------------------------------------------------------------------------------------

/// What to do about a worker that ended with an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    /// Start it again after this delay.
    RestartAfter(Duration),
    /// Retrying cannot help; leave this account stopped.
    GiveUp,
}

/// Decides whether a worker that failed is restarted, and when.
///
/// `failures` counts consecutive failures including this one (1 for the first).
///
/// Only a rejected login is restarted. It is "fatal" for the worker's own fast retry loop, but
/// it can be transient on the server side (a password being rotated, a brief auth backend
/// outage), so we try again, slowly: a wrong password must never be hammered, since many
/// servers lock the account. Everything else that is fatal is a local problem that a restart
/// would only repeat: a live account without custom keywords, a missing password, a broken database,
/// an insecure-transport refusal, a worker panic.
pub fn restart_decision(
    error: &WorkerError,
    failures: u32,
    timing: &SupervisorTiming,
) -> RestartDecision {
    match error {
        WorkerError::Imap(ImapError::Login(_)) => RestartDecision::RestartAfter(backoff_delay(
            failures,
            timing.restart_initial,
            timing.restart_max,
        )),
        _ => RestartDecision::GiveUp,
    }
}

// ---------------------------------------------------------------------------------------------
// One account, kept alive
// ---------------------------------------------------------------------------------------------

/// How an account's supervision ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountEnd {
    /// Shutdown was requested and the worker stopped cleanly.
    Stopped,
    /// The worker failed in a way a restart cannot fix. Holds the error text.
    GaveUp(String),
}

/// Runs a worker until shutdown, restarting it after restartable failures.
///
/// `make_worker` is called once per start and returns the worker's future. Taking a closure
/// instead of a future is what makes restarting possible: a future can only be run once.
/// (`FnMut() -> Fut` reads "a function I can call repeatedly, each call giving a new `Fut`".)
pub async fn supervise<F, Fut>(
    name: &str,
    shutdown: &CancellationToken,
    timing: &SupervisorTiming,
    mut make_worker: F,
) -> AccountEnd
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), WorkerError>>,
{
    let mut failures = 0u32;
    loop {
        let started = Instant::now();
        let error = match make_worker().await {
            Ok(()) => return AccountEnd::Stopped,
            Err(error) => error,
        };
        // A worker that ran fine for longer than the longest wait was healthy: start the
        // backoff from the beginning, so one bad login after days of work waits 5 min, not 1 h.
        if started.elapsed() >= timing.restart_max {
            failures = 0;
        }
        failures += 1;
        match restart_decision(&error, failures, timing) {
            RestartDecision::GiveUp => {
                tracing::error!(account = %name, %error, "worker failed; not restarting");
                return AccountEnd::GaveUp(error.to_string());
            }
            RestartDecision::RestartAfter(delay) => {
                tracing::error!(
                    account = %name, %error, attempt = failures,
                    restart_in_secs = delay.as_secs(),
                    "worker failed; will restart (check the account settings)"
                );
                // Sleep, but wake at once if shutdown arrives meanwhile.
                tokio::select! {
                    () = sleep(delay) => {}
                    () = shutdown.cancelled() => return AccountEnd::Stopped,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// All accounts
// ---------------------------------------------------------------------------------------------

/// The result of [`serve`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ServiceReport {
    /// How many accounts were started.
    pub accounts: usize,
    /// Accounts that ended with an error nobody can fix by restarting, with the error text.
    pub gave_up: Vec<(String, String)>,
    /// How many tasks were still running after the grace period and were aborted.
    pub aborted: usize,
}

impl ServiceReport {
    /// True when no account is left running, which is the only reason `run` exits non-zero.
    pub fn every_account_failed(&self) -> bool {
        self.accounts > 0 && self.gave_up.len() == self.accounts
    }
}

/// Starts `unit(index)` for every account as its own task and waits for all of them.
///
/// `names` are the account names, used for logging. When `shutdown` fires, the tasks get
/// `timing.grace` to finish, then the rest are aborted. One account failing does not touch
/// the others: each task has its own result.
pub async fn serve<F, Fut>(
    names: &[String],
    shutdown: &CancellationToken,
    timing: &SupervisorTiming,
    unit: F,
) -> ServiceReport
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = AccountEnd> + Send + 'static,
{
    let mut report = ServiceReport {
        accounts: names.len(),
        ..ServiceReport::default()
    };

    // `spawn` returns an `AbortHandle`; its id lets us name the account of a finished task,
    // even when the task panicked and returned nothing.
    let mut set = JoinSet::new();
    let mut ids = HashMap::new();
    for (index, name) in names.iter().enumerate() {
        let handle = set.spawn(unit(index));
        ids.insert(handle.id(), name.clone());
    }

    // Starts counting only once shutdown is requested.
    let grace_timer = async {
        shutdown.cancelled().await;
        sleep(timing.grace).await;
    };
    tokio::pin!(grace_timer);
    let mut grace_over = false;

    loop {
        tokio::select! {
            joined = set.join_next_with_id() => {
                let Some(joined) = joined else { break };
                match joined {
                    Ok((_, AccountEnd::Stopped)) => {}
                    Ok((id, AccountEnd::GaveUp(reason))) => {
                        report.gave_up.push((ids[&id].clone(), reason));
                    }
                    // Cancelled by `abort_all` below; already counted.
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => {
                        let account = ids[&error.id()].clone();
                        tracing::error!(%account, %error, "account task panicked");
                        report.gave_up.push((account, error.to_string()));
                    }
                }
            }
            // The `if` guard switches this branch off after it fired once: a finished
            // future must not be polled again.
            () = &mut grace_timer, if !grace_over => {
                grace_over = true;
                report.aborted = set.len();
                tracing::warn!(
                    still_running = report.aborted,
                    "grace period over; aborting the remaining tasks"
                );
                set.abort_all();
            }
        }
    }
    report
}

// ---------------------------------------------------------------------------------------------
// The real service
// ---------------------------------------------------------------------------------------------

/// Runs the inbox worker and the Sent harvest of every account until `shutdown` fires.
///
/// The config and the decider are shared with all tasks through `Arc`: cloning an `Arc` copies
/// a pointer and bumps a counter, not the data. Tasks must own what they use (`'static`),
/// because a spawned task can outlive the function that started it.
pub async fn run_service<D: Decider + Send + Sync + 'static>(
    config: Arc<Config>,
    decider: Arc<D>,
    store: Arc<Mutex<Store>>,
    shutdown: CancellationToken,
    timing: SupervisorTiming,
) -> ServiceReport {
    let names: Vec<String> = config.accounts.iter().map(|a| a.name.clone()).collect();
    let timing = Arc::new(timing);
    serve(&names, &shutdown, &timing, |index| {
        account_unit(
            Arc::clone(&config),
            index,
            Arc::clone(&decider),
            Arc::clone(&store),
            shutdown.clone(),
            Arc::clone(&timing),
        )
    })
    .await
}

/// One account: the supervised inbox worker next to the Sent harvester.
async fn account_unit<D: Decider + Send + Sync>(
    config: Arc<Config>,
    index: usize,
    decider: Arc<D>,
    store: Arc<Mutex<Store>>,
    shutdown: CancellationToken,
    timing: Arc<SupervisorTiming>,
) -> AccountEnd {
    let account = &config.accounts[index];
    // A child token fires when the parent does, and can also be cancelled on its own. The
    // harvester must stop when the inbox worker gives up, or this task would never end.
    let harvest_stop = shutdown.child_token();

    let inbox = async {
        let end = supervise(&account.name, &shutdown, &timing, || {
            run_account(
                account,
                &config,
                Arc::clone(&decider),
                Arc::clone(&store),
                shutdown.clone(),
            )
        })
        .await;
        harvest_stop.cancel();
        end
    };
    let harvest = harvest_loop(account, &config, &store, &harvest_stop, &timing);

    // `join!` runs both futures concurrently inside this one task, so aborting the task
    // stops both. The harvester returns `()`, so only the inbox result is kept.
    let (end, ()) = tokio::join!(inbox, harvest);
    end
}

/// Harvests the Sent folder now and then, until `stop` fires. A failure is logged and retried
/// later; it never affects the inbox worker.
async fn harvest_loop(
    account: &AccountConfig,
    config: &Config,
    store: &Arc<Mutex<Store>>,
    stop: &CancellationToken,
    timing: &SupervisorTiming,
) {
    if !config.policy.known_senders_from_sent {
        return;
    }
    loop {
        let wait = tokio::select! {
            result = harvest_once(account, config, store) => match result {
                Ok(()) => timing.harvest_interval,
                Err(message) => {
                    tracing::warn!(
                        account = %account.name, error = %message,
                        retry_in_secs = timing.harvest_retry.as_secs(),
                        "Sent harvest failed; the inbox worker is not affected"
                    );
                    timing.harvest_retry
                }
            },
            () = stop.cancelled() => return,
        };
        tokio::select! {
            () = sleep(wait) => {}
            () = stop.cancelled() => return,
        }
    }
}

/// One harvest on its own IMAP connection. Errors are turned into text: the caller only logs.
async fn harvest_once(
    account: &AccountConfig,
    config: &Config,
    store: &Arc<Mutex<Store>>,
) -> Result<(), String> {
    // `password_command` runs a shell command and waits, so it goes to a blocking thread.
    let for_password = account.clone();
    let password = tokio::task::spawn_blocking(move || for_password.password())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let mut session = ImapSession::connect(&account.imap_settings(&password))
        .await
        .map_err(|e| e.to_string())?;
    let result = harvest_known_senders(
        &mut session,
        store,
        account,
        config.policy.known_senders_from_sent,
    )
    .await;
    // Best effort: the connection is dropped either way.
    let _ = session.logout().await;
    result.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn timing() -> SupervisorTiming {
        SupervisorTiming::default()
    }

    fn login_error() -> WorkerError {
        WorkerError::Imap(ImapError::Login(
            std::io::Error::other("bad password").into(),
        ))
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rejected_login_restarts_with_doubling_delay_up_to_the_cap() {
        let delays: Vec<u64> = (1..=6)
            .map(|n| match restart_decision(&login_error(), n, &timing()) {
                RestartDecision::RestartAfter(d) => d.as_secs() / 60,
                RestartDecision::GiveUp => panic!("login errors restart"),
            })
            .collect();
        assert_eq!(delays, [5, 10, 20, 40, 60, 60]);
    }

    #[test]
    fn local_problems_are_never_restarted() {
        for error in [
            WorkerError::LiveWithoutKeywords {
                account: "a".into(),
            },
            WorkerError::Config(crate::config::ConfigError::PasswordMissing {
                account: "a".into(),
                env_var: "X".into(),
            }),
            WorkerError::Imap(ImapError::StartTlsUnsupported),
            WorkerError::Imap(ImapError::KeywordsNotAllowed),
        ] {
            assert_eq!(
                restart_decision(&error, 1, &timing()),
                RestartDecision::GiveUp,
                "{error}"
            );
        }
    }

    // `start_paused` makes tokio's clock jump forward whenever every task is waiting, so a
    // five-minute restart delay takes no real time in these tests.
    #[tokio::test(start_paused = true)]
    async fn supervise_restarts_after_a_login_failure_then_stops_on_shutdown() {
        let shutdown = CancellationToken::new();
        let starts = Arc::new(AtomicU32::new(0));
        let end = supervise("a", &shutdown, &timing(), || {
            let (starts, shutdown) = (Arc::clone(&starts), shutdown.clone());
            async move {
                match starts.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(login_error()),
                    _ => {
                        shutdown.cancel();
                        Ok(())
                    }
                }
            }
        })
        .await;
        assert_eq!(end, AccountEnd::Stopped);
        assert_eq!(starts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_healthy_run_resets_the_restart_backoff() {
        let shutdown = CancellationToken::new();
        let timing = timing();
        let starts = Arc::new(Mutex::new(Vec::new()));
        supervise("a", &shutdown, &timing, || {
            let (starts, shutdown) = (Arc::clone(&starts), shutdown.clone());
            let healthy_for = timing.restart_max;
            async move {
                let attempt = {
                    let mut starts = starts.lock().unwrap();
                    starts.push(Instant::now());
                    starts.len()
                };
                match attempt {
                    1 | 2 => Err(login_error()),
                    3 => {
                        // Runs fine for a long time, then the login fails once more.
                        sleep(healthy_for).await;
                        Err(login_error())
                    }
                    _ => {
                        shutdown.cancel();
                        Ok(())
                    }
                }
            }
        })
        .await;
        let starts = starts.lock().unwrap();
        assert_eq!(starts.len(), 4);
        // Second failure in a row: the doubled wait.
        assert_eq!(starts[2] - starts[1], timing.restart_initial * 2);
        // After the healthy run the wait starts from the beginning again.
        assert_eq!(
            starts[3] - starts[2],
            timing.restart_max + timing.restart_initial
        );
    }

    #[tokio::test(start_paused = true)]
    async fn supervise_gives_up_on_non_restartable_errors() {
        let shutdown = CancellationToken::new();
        let end = supervise("a", &shutdown, &timing(), || async {
            Err(WorkerError::LiveWithoutKeywords {
                account: "a".into(),
            })
        })
        .await;
        assert!(matches!(end, AccountEnd::GaveUp(reason) if reason.contains("custom keywords")));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_the_restart_wait_stops_at_once() {
        let shutdown = CancellationToken::new();
        let starts = Arc::new(AtomicU32::new(0));
        let canceller = shutdown.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(1)).await;
            canceller.cancel();
        });
        let end = supervise("a", &shutdown, &timing(), || {
            starts.fetch_add(1, Ordering::SeqCst);
            async { Err(login_error()) }
        })
        .await;
        assert_eq!(end, AccountEnd::Stopped);
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "no restart after shutdown"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn one_failing_account_does_not_stop_the_other() {
        let shutdown = CancellationToken::new();
        let canceller = shutdown.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(60)).await;
            canceller.cancel();
        });
        let token = shutdown.clone();
        let report = serve(&names(&["bad", "good"]), &shutdown, &timing(), |index| {
            let token = token.clone();
            async move {
                if index == 0 {
                    AccountEnd::GaveUp("boom".into())
                } else {
                    token.cancelled().await;
                    AccountEnd::Stopped
                }
            }
        })
        .await;
        assert_eq!(report.accounts, 2);
        assert_eq!(report.gave_up, [("bad".to_string(), "boom".to_string())]);
        assert!(!report.every_account_failed());
        assert_eq!(report.aborted, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn all_accounts_failing_is_reported() {
        let shutdown = CancellationToken::new();
        let report = serve(&names(&["a", "b"]), &shutdown, &timing(), |_| async {
            AccountEnd::GaveUp("x".into())
        })
        .await;
        assert!(report.every_account_failed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_task_that_ignores_shutdown_is_aborted_after_the_grace_period() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let report = serve(&names(&["stuck"]), &shutdown, &timing(), |_| {
            // Never finishes on its own.
            std::future::pending::<AccountEnd>()
        })
        .await;
        assert_eq!(report.aborted, 1);
        assert!(report.gave_up.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_task_is_reported_for_its_account() {
        let shutdown = CancellationToken::new();
        let report = serve(&names(&["p"]), &shutdown, &timing(), |_| async {
            if true {
                panic!("worker bug");
            }
            AccountEnd::Stopped
        })
        .await;
        assert_eq!(report.gave_up.len(), 1);
        assert_eq!(report.gave_up[0].0, "p");
    }
}
