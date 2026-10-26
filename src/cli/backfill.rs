//! The `backfill` command: run the normal pipeline over inbox mail from the last few days.
//!
//! `run` only looks at mail that arrives while it is running (and ignores the history on the
//! first start, on purpose). `backfill` is for catching up on purpose, for example after
//! installing the service, or after switching an account from dry-run to live. It is the same
//! code path as the live worker ([`lets_triage_mail::imap::worker::backfill_account`]), so the
//! same rules hold: dry-run accounts only log, `\Flagged` mail is left alone, already
//! processed mail is skipped, and the worker's saved position is never touched.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use jiff::Span;
use jiff::civil::Date;
use lets_triage_mail::config::{AccountConfig, Config, FlagMode};
use lets_triage_mail::decider::{LlmDecider, LlmSettings};
use lets_triage_mail::imap::worker::{BackfillReport, backfill_account};
use lets_triage_mail::store::Store;
use tokio_util::sync::CancellationToken;

/// Classifies and flags existing inbox mail from the last `days` days.
///
/// `account` limits the run to one configured account; `None` means all of them, one after
/// the other. A failing account does not stop the others; the command then exits non-zero.
pub async fn run(config_path: &Path, days: u32, account: Option<&str>) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;
    let accounts = super::check::select_accounts(&config, account)?;
    let since = since_date(jiff::Zoned::now().date(), days)?;

    // The same shared pieces as `run`: one decider (which holds the model concurrency limit)
    // and one store.
    let decider = Arc::new(
        LlmDecider::new(LlmSettings::from(&config.llm))
            .context("could not set up the model client")?,
    );
    let store = Arc::new(Mutex::new(
        Store::open(&config.db_path()).context("could not open the state database")?,
    ));

    // Ctrl-C stops after the message in progress; a second one exits at once.
    let shutdown = CancellationToken::new();
    tokio::spawn(super::run::watch_for_signals(shutdown.clone()));

    let mut failed = Vec::new();
    for account in accounts {
        if shutdown.is_cancelled() {
            break;
        }
        println!("{}", describe_start(&config, account, since));
        let result = backfill_account(
            account,
            &config,
            Arc::clone(&decider),
            Arc::clone(&store),
            shutdown.clone(),
            since,
        )
        .await;
        match result {
            Ok(report) => println!("{}", format_report(&account.name, &report)),
            Err(error) => {
                eprintln!("{}: backfill failed: {error}", account.name);
                failed.push(account.name.clone());
            }
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("backfill failed for: {}", failed.join(", "))
    }
}

/// The first day to look at: `days` days before `today`. `--days 0` means today only.
fn since_date(today: Date, days: u32) -> anyhow::Result<Date> {
    // `try_days` and `checked_sub` return errors for absurd windows; the shorter `.days()`
    // helper would panic on them, and a typo in a command-line flag must not panic.
    let window = Span::new()
        .try_days(i64::from(days))
        .with_context(|| format!("--days {days} is too large"))?;
    today
        .checked_sub(window)
        .with_context(|| format!("--days {days} reaches back before the supported dates"))
}

/// What is about to happen to this account, in plain words.
fn describe_start(config: &Config, account: &AccountConfig, since: Date) -> String {
    let mode = account.flag_mode(&config.policy);
    let effect = match mode {
        FlagMode::Live => "LIVE: flags will be set on the server",
        FlagMode::DryRun => "dry-run: nothing will be changed",
        FlagMode::ClassifyOnly => "classify-only: mail is classified, never flagged",
    };
    format!(
        "{}: backfilling inbox mail since {since} ({effect})",
        account.name
    )
}

/// One summary line for a finished (or interrupted) account.
fn format_report(account: &str, report: &BackfillReport) -> String {
    let mut line = format!(
        "{account}: {} messages found: {} processed, {} skipped",
        report.found, report.processed, report.skipped
    );
    if report.deferred > 0 {
        line.push_str(&format!(
            ", {} deferred (the model failed; they are queued and the running service retries them)",
            report.deferred
        ));
    }
    if report.gone > 0 {
        line.push_str(&format!(", {} vanished meanwhile", report.gone));
    }
    if report.interrupted {
        line.push_str(" (interrupted; run again to continue, finished messages are skipped)");
    }
    line
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    #[test]
    fn since_counts_back_whole_days() {
        let today = date(2026, 10, 26);
        assert_eq!(since_date(today, 7).unwrap(), date(2026, 10, 19));
        assert_eq!(since_date(today, 0).unwrap(), today);
        // Across a month and a year boundary.
        assert_eq!(since_date(today, 30).unwrap(), date(2026, 9, 26));
        assert_eq!(since_date(date(2027, 1, 3), 5).unwrap(), date(2026, 12, 29));
    }

    #[test]
    fn an_absurd_window_is_an_error_not_a_panic() {
        assert!(since_date(date(2026, 10, 26), u32::MAX).is_err());
    }

    #[test]
    fn the_report_line_mentions_only_what_happened() {
        let plain = BackfillReport {
            found: 5,
            processed: 4,
            skipped: 1,
            ..BackfillReport::default()
        };
        assert_eq!(
            format_report("zoho", &plain),
            "zoho: 5 messages found: 4 processed, 1 skipped"
        );
        let busy = BackfillReport {
            deferred: 2,
            gone: 1,
            interrupted: true,
            ..plain
        };
        let text = format_report("zoho", &busy);
        assert!(text.contains("2 deferred"), "{text}");
        assert!(text.contains("1 vanished"), "{text}");
        assert!(text.contains("interrupted"), "{text}");
    }
}
