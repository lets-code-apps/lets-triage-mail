//! The `run` command: watch every account, classify new mail and log what it would flag.
//!
//! Only dry-run exists so far (phase 4 adds the real flagging), so nothing here changes
//! anything on a mail server. This file owns the process-level concerns: the start-up checks,
//! the logging of what is about to happen, the signal handling and the exit code. The task
//! management lives in [`lets_triage_mail::supervisor`].

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use lets_triage_mail::config::Config;
use lets_triage_mail::decider::{LlmDecider, LlmSettings};
use lets_triage_mail::store::Store;
use lets_triage_mail::supervisor::{SupervisorTiming, run_service};
use tokio_util::sync::CancellationToken;

/// Exit status when a second signal forces an immediate exit (128 + SIGINT, the shell
/// convention for "killed by Ctrl-C").
const FORCED_EXIT_CODE: i32 = 130;

/// Watches the accounts until Ctrl-C or SIGTERM.
pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;
    ensure_dry_run(&config)?;

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

/// Refuses to start unless `dry_run` is on, before any network connection is made.
///
/// The worker checks this too; failing here gives the message straight away instead of one
/// error per account.
fn ensure_dry_run(config: &Config) -> anyhow::Result<()> {
    if config.policy.dry_run {
        return Ok(());
    }
    anyhow::bail!(
        "`policy.dry_run = false` is not supported yet: flagging arrives in phase 4. \
         Set `dry_run = true` in the config; `run` then only logs what it would flag"
    )
}

/// One `info` block describing what is about to run. Never includes passwords or API keys.
fn log_startup_summary(config: &Config) {
    let accounts: Vec<String> = config
        .accounts
        .iter()
        .map(|a| format!("{} ({}@{}:{})", a.name, a.username, a.host, a.port()))
        .collect();
    tracing::info!(
        accounts = %accounts.join(", "),
        model = %config.llm.model,
        model_url = %config.llm.base_url,
        database = %config.db_path().display(),
        "starting lets-triage-mail"
    );
    tracing::info!("dry-run: nothing will be changed on the server");
}

/// Cancels `shutdown` on the first Ctrl-C or SIGTERM, and exits at once on the second.
async fn watch_for_signals(shutdown: CancellationToken) {
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

    fn config(dry_run: bool) -> Config {
        Config::parse(&format!(
            "[policy]\ndry_run = {dry_run}\n\
             [[accounts]]\nname = \"a\"\nhost = \"127.0.0.1\"\nusername = \"u\"\ntls = \"none\"\n"
        ))
        .unwrap()
    }

    #[test]
    fn dry_run_is_accepted() {
        assert!(ensure_dry_run(&config(true)).is_ok());
    }

    #[test]
    fn live_mode_is_refused_with_a_pointer_to_phase_4() {
        let error = ensure_dry_run(&config(false)).unwrap_err().to_string();
        assert!(error.contains("phase 4"), "{error}");
        assert!(error.contains("dry_run = true"), "{error}");
    }
}
