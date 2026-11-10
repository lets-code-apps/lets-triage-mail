//! The `run` command: watch every account, classify new mail, and set and clear flags.
//!
//! Whether an account's mail is really flagged or only logged is decided per account (see
//! `AccountConfig::flag_mode`), so one account can be live while the others stay in dry-run.
//! This file owns the process-level concerns: the logging of what is about to happen, the
//! signal handling and the exit code. The task management lives in
//! [`lets_triage_mail::supervisor`], the mailbox work in [`lets_triage_mail::imap::worker`].

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use lets_triage_mail::config::{Config, FlagMode};
use lets_triage_mail::decider::{LlmDecider, LlmSettings};
use lets_triage_mail::store::Store;
use lets_triage_mail::supervisor::{SupervisorTiming, run_service};
use tokio_util::sync::CancellationToken;

/// Exit status when a second signal forces an immediate exit (128 + SIGINT, the shell
/// convention for "killed by Ctrl-C").
const FORCED_EXIT_CODE: i32 = 130;

/// Exit status when another engine already holds the lock (`EX_TEMPFAIL`). The desktop app reads
/// it to avoid restarting an engine that would only fail again.
const ALREADY_RUNNING_EXIT_CODE: i32 = 75;

/// Takes the single-instance lock for `db_path`: an exclusive `flock` on `<db_path>.lock`.
/// Keep the returned file alive for the whole run. The OS drops the lock when the process
/// ends, even on a crash or `kill -9`, so there is never a stale lock to clean up.
fn lock_instance(db_path: &Path) -> anyhow::Result<Result<File, ()>> {
    let mut lock_path = db_path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    if let Some(dir) = lock_path.parent() {
        lets_triage_mail::store::create_private_dir(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("could not open {}", lock_path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Ok(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(Err(())),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("could not lock {}", lock_path.display()))
        }
    }
}

/// Watches the accounts until Ctrl-C or SIGTERM (or, with `exit_on_stdin_close`, stdin EOF).
pub async fn run(config_path: &Path, exit_on_stdin_close: bool) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;

    // Held until `run` returns. Only `run` locks, so `status`, `check` and `digest` keep working.
    let _instance_lock = match lock_instance(&config.db_path())? {
        Ok(file) => file,
        Err(()) => {
            tracing::error!(
                "another lets-triage-mail engine is already running for {}",
                config.db_path().display()
            );
            std::process::exit(ALREADY_RUNNING_EXIT_CODE);
        }
    };

    // Built once and shared by every account worker. `Arc` is a reference-counted pointer:
    // cloning it hands the same decider or store to another task without copying it.
    let decider = Arc::new(
        LlmDecider::new(LlmSettings::from(&config.llm))
            .context("could not set up the model client")?,
    );
    // `std::sync::Mutex` is enough: the store is only ever locked for short, blocking
    // database calls, never held across an `.await`.
    let store = Arc::new(Mutex::new(
        Store::open(&config.db_path()).context("could not open the state database")?,
    ));

    log_startup_summary(&config);

    let shutdown = CancellationToken::new();
    tokio::spawn(watch_for_signals(shutdown.clone()));
    if exit_on_stdin_close {
        shutdown_on_stdin_eof(shutdown.clone());
    }

    let report = run_service(
        Arc::new(config),
        decider,
        store,
        shutdown,
        SupervisorTiming::default(),
    )
    .await;

    if report.aborted > 0 {
        tracing::warn!(
            tasks = report.aborted,
            "stopped; some tasks had to be aborted"
        );
    } else {
        tracing::info!("stopped cleanly");
    }
    if report.every_account_failed() {
        let details: Vec<String> = report
            .gave_up
            .iter()
            .map(|(account, reason)| format!("{account}: {reason}"))
            .collect();
        anyhow::bail!("no account could be started ({})", details.join("; "));
    }
    Ok(())
}

/// One `info` block describing what is about to run. Never includes passwords or API keys.
fn log_startup_summary(config: &Config) {
    let accounts: Vec<String> = config
        .accounts
        .iter()
        .map(|a| {
            format!(
                "{} ({}@{}:{}, {})",
                a.name,
                a.username,
                a.host,
                a.port(),
                a.flag_mode(&config.policy).as_str()
            )
        })
        .collect();
    tracing::info!(
        accounts = %accounts.join(", "),
        model = %config.llm.model,
        model_url = %config.llm.base_url,
        database = %config.db_path().display(),
        "starting lets-triage-mail"
    );
    for line in mode_summary(config) {
        tracing::info!("{line}");
    }
}

/// One line per kind of account mode, saying in plain words what will happen. Live accounts
/// are logged at the top so nobody is surprised that flags will appear.
fn mode_summary(config: &Config) -> Vec<String> {
    let names = |mode: FlagMode| -> Vec<&str> {
        config
            .accounts
            .iter()
            .filter(|a| a.flag_mode(&config.policy) == mode)
            .map(|a| a.name.as_str())
            .collect()
    };
    let mut lines = Vec::new();
    let live = names(FlagMode::Live);
    if !live.is_empty() {
        lines.push(format!(
            "LIVE: flags will be set and cleared on the server for: {}",
            live.join(", ")
        ));
    }
    let dry = names(FlagMode::DryRun);
    if !dry.is_empty() {
        lines.push(format!(
            "dry-run: nothing will be changed on the server for: {}",
            dry.join(", ")
        ));
    }
    let classify_only = names(FlagMode::ClassifyOnly);
    if !classify_only.is_empty() {
        lines.push(format!(
            "classify-only (flagging = false): mail is classified for the digest, never flagged, for: {}",
            classify_only.join(", ")
        ));
    }
    lines
}

/// Cancels `shutdown` (the graceful path, like a first SIGTERM) once stdin hits end-of-file.
/// The desktop holds the pipe's write end, so EOF means the app is gone.
///
/// A plain thread, not a tokio task: a blocking stdin read inside the runtime would keep the
/// process alive after a SIGTERM shutdown (the runtime waits for blocking tasks on drop).
fn shutdown_on_stdin_eof(shutdown: CancellationToken) {
    std::thread::spawn(move || {
        // Any read error also ends the wait: a broken stdin is as good as a closed one.
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        // Cancel first: the app that read our stderr is usually gone too, so logging may fail.
        shutdown.cancel();
        tracing::info!("stdin closed: shutting down");
    });
}

/// Cancels `shutdown` on the first Ctrl-C or SIGTERM, and exits at once on the second.
pub(super) async fn watch_for_signals(shutdown: CancellationToken) {
    wait_for_signal().await;
    tracing::info!("shutting down (press Ctrl-C again to exit immediately)");
    shutdown.cancel();
    wait_for_signal().await;
    tracing::warn!("second signal: exiting immediately");
    // Skips the grace period and the destructors on purpose: the user asked for it.
    std::process::exit(FORCED_EXIT_CODE);
}

/// Resolves when Ctrl-C (all platforms) or SIGTERM (Unix; what `launchd` and `kill` send) arrives.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // Registering can only fail if the OS refuses; then Ctrl-C alone still works.
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str =
        "[[accounts]]\nname = \"{n}\"\nhost = \"127.0.0.1\"\nusername = \"u\"\ntls = \"none\"\n";

    fn account(name: &str, extra: &str) -> String {
        format!("{}{extra}", ACCOUNT.replace("{n}", name))
    }

    #[test]
    fn a_second_lock_on_the_same_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let _first = lock_instance(&db).unwrap().expect("first lock is free");
        assert!(
            lock_instance(&db).unwrap().is_err(),
            "second must say already running"
        );
    }

    #[test]
    fn the_lock_is_released_when_the_holder_drops() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        drop(lock_instance(&db).unwrap().unwrap());
        assert!(lock_instance(&db).unwrap().is_ok());
    }

    #[test]
    fn the_summary_names_each_mode_and_its_accounts() {
        let config = Config::parse(&format!(
            "{}{}{}",
            account("zoho", "dry_run = false\n"),
            account("gmail", ""),
            account("wp", "flagging = false\n"),
        ))
        .unwrap();
        let lines = mode_summary(&config);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].starts_with("LIVE") && lines[0].ends_with("zoho"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("dry-run") && lines[1].ends_with("gmail"),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with("classify-only") && lines[2].ends_with("wp"),
            "{lines:?}"
        );
    }

    #[test]
    fn an_all_dry_run_config_says_nothing_is_live() {
        let config = Config::parse(&account("a", "")).unwrap();
        let lines = mode_summary(&config);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("LIVE"), "{lines:?}");
    }

    #[test]
    fn a_live_policy_with_one_account_held_back() {
        let config = Config::parse(&format!(
            "[policy]\ndry_run = false\n{}{}",
            account("a", ""),
            account("b", "dry_run = true\n"),
        ))
        .unwrap();
        let lines = mode_summary(&config);
        assert!(
            lines[0].contains("LIVE") && lines[0].ends_with('a'),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("dry-run") && lines[1].ends_with('b'),
            "{lines:?}"
        );
    }
}
