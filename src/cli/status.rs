//! The `status` command: one snapshot of what the engine has done today, for machines.
//!
//! Like `stats`, the command function only does IO (open the database, ask the clock, print);
//! the shaping and the text live in [`lets_triage_mail::status`], whose JSON shape is locked
//! by a test there because the desktop app parses this stdout. `--json` prints that shape;
//! plain text is the human view of the same report.

use std::path::Path;

use anyhow::Context;
use lets_triage_mail::config::Config;
use lets_triage_mail::status::{RawStatus, build_report, format_text, start_of_day};
use lets_triage_mail::store::Store;

/// Prints the engine's liveness, today's counts and every account's state.
pub async fn run(config_path: &Path, json: bool) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;
    let db_path = config.db_path();
    // `Store` blocks the thread, so it runs on tokio's blocking pool (see `store.rs`).
    let output = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let store = Store::open(&db_path)
            .with_context(|| format!("cannot open the database {}", db_path.display()))?;
        // One clock reading: `since` is "today" in the machine's own zone, exactly like
        // the digest's idea of today (see `status.rs`).
        let now_zoned = jiff::Zoned::now();
        let now = now_zoned.timestamp().as_second();
        let since = start_of_day(now_zoned.timestamp(), now_zoned.time_zone().clone());
        let raw = query(&store, &config, since, now)?;
        let report = build_report(now, since, &config, raw);
        if json {
            Ok(serde_json::to_string_pretty(&report)?)
        } else {
            Ok(format_text(&report))
        }
    })
    .await??;
    println!("{output}");
    Ok(())
}

/// Reads everything the report needs from the store.
fn query(store: &Store, config: &Config, since: i64, now: i64) -> anyhow::Result<RawStatus> {
    let rows = config
        .accounts
        .iter()
        .map(|account| store.digest_rows(account.name.as_str(), since, now))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RawStatus {
        engine: store.engine_status()?,
        states: store.account_states()?,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use lets_triage_mail::config::Config;
    use lets_triage_mail::store::Store;

    use super::query;

    #[test]
    fn query_reads_a_real_store_even_when_it_is_empty() {
        let config = Config::parse(
            r#"
            [[accounts]]
            name = "work"
            host = "imap.example.com"
            username = "me@example.com"
            "#,
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        let raw = query(&store, &config, 0, 1_000).unwrap();
        assert!(raw.engine.is_none());
        assert!(raw.states.is_empty());
        assert_eq!(raw.rows.len(), 1);
    }
}
