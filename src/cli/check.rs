//! The `check` command: test the config, the model and each account, without changing
//! anything on any mail server.
//!
//! The probes and the report formatting live in [`lets_triage_mail::check`]; this file decides
//! what to run, in which order, and prints. Accounts are checked one after another, and a
//! failing account never stops the others.

use std::path::Path;

use lets_triage_mail::check::{self, CheckItem, Report};
use lets_triage_mail::config::{AccountConfig, Config};

/// Runs the checks and prints the report. Fails (non-zero exit code) if anything failed.
pub async fn run(config_path: &Path, account: Option<&str>, json: bool) -> anyhow::Result<()> {
    let mut report = Report::default();

    let config = match super::load_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            // A config that does not load is itself a check result, not a crash.
            let section = vec![CheckItem::fail(
                "Config",
                format!("{error:#}"),
                "fix the config file (see config.example.toml) and run `check` again",
            )];
            finish_section(&mut report, section, json);
            return conclude(&report, json);
        }
    };

    let accounts = select_accounts(&config, account)?;

    let mut section = vec![CheckItem::ok(
        "Config",
        format!("{} loads and validates", config_path.display()),
    )];
    section.extend(check::mode_items(&config));
    section.push(check::database_item(&config.db_path()));
    finish_section(&mut report, section, json);

    finish_section(&mut report, check::probe_model(&config.llm).await, json);

    for account in accounts {
        finish_section(&mut report, check::probe_account(account).await, json);
    }

    conclude(&report, json)
}

/// Adds a finished section to the report. Text mode prints it right away, so a slow account
/// does not hide the results before it; JSON mode needs the whole report and prints at the end.
fn finish_section(report: &mut Report, section: Vec<CheckItem>, json: bool) {
    if !json {
        if !report.items.is_empty() {
            println!();
        }
        let printable = Report { items: section };
        print!("{}", printable.to_text());
        report.items.extend(printable.items);
    } else {
        report.items.extend(section);
    }
}

/// The accounts to check: the named one, or all of them.
pub(super) fn select_accounts<'a>(
    config: &'a Config,
    name: Option<&str>,
) -> anyhow::Result<Vec<&'a AccountConfig>> {
    let Some(name) = name else {
        return Ok(config.accounts.iter().collect());
    };
    match config.account(name) {
        Some(account) => Ok(vec![account]),
        None => {
            let known: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
            anyhow::bail!(
                "no account named {name:?} in the config (accounts: {})",
                known.join(", ")
            )
        }
    }
}

/// Prints the closing output and turns failures into the command's error.
fn conclude(report: &Report, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", report.to_json());
    } else {
        println!("\n{}", report.summary());
    }
    if report.has_failures() {
        // The details are in the report above; this only sets the exit code.
        anyhow::bail!("check failed");
    }
    Ok(())
}
