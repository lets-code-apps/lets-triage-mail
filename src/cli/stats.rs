//! The `stats` command: what the service has done in the last few days.
//!
//! The command function only does IO (open the database, ask the clock, print). The
//! numbers are assembled by [`build_report`] and printed by [`format_report`], both pure
//! functions, so tests can feed them plain values.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use lets_triage_mail::config::Config;
use lets_triage_mail::store::{AccountCount, Counted, RetrySummary, Store};
use serde::Serialize;

/// How many senders the "top senders" lists show.
const TOP_SENDERS: u32 = 10;
const SECONDS_PER_DAY: i64 = 86_400;

/// One line of a table: a name and how often it occurred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Count {
    name: String,
    count: u64,
}

/// The numbers for one account, or for all of them together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct AccountStats {
    account: String,
    messages: u64,
    categories: Vec<Count>,
    flags: Vec<Count>,
    top_senders: Vec<Count>,
    /// Messages waiting for another model attempt (model failures).
    retry_queued: u64,
    /// Of those, how many are due now.
    retry_due: u64,
}

/// Everything `stats` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Report {
    days: u32,
    /// Whether flags are only logged (`true`) or really set on the server.
    dry_run: bool,
    /// First and last day (`YYYY-MM-DD`, UTC) with a logged message in the period.
    first_day: Option<String>,
    last_day: Option<String>,
    accounts: Vec<AccountStats>,
    total: AccountStats,
}

/// The raw rows the store returned, before they are shaped into a [`Report`].
struct RawStats {
    categories: Vec<AccountCount>,
    flags: Vec<AccountCount>,
    retries: Vec<RetrySummary>,
    range: Option<(i64, i64)>,
    top_senders_total: Vec<Counted>,
    /// Top senders of each configured account, in the same order as the config.
    top_senders: Vec<Vec<Counted>>,
}

/// Shows what the service did in the last `days` days.
pub async fn run(config_path: &Path, days: u32, json: bool) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;
    let db_path = config.db_path();
    // `Store` blocks the thread, so it runs on tokio's blocking pool (see `store.rs`).
    let output = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let store = Store::open(&db_path)
            .with_context(|| format!("cannot open the database {}", db_path.display()))?;
        let now = unix_now();
        let raw = query(&store, &config, since(now, days), now)?;
        let names: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
        let report = build_report(days, config.policy.dry_run, &names, raw);
        if json {
            Ok(serde_json::to_string_pretty(&report)?)
        } else {
            Ok(format_report(&report))
        }
    })
    .await??;
    println!("{output}");
    Ok(())
}

/// Reads everything the report needs from the store.
fn query(store: &Store, config: &Config, since: i64, now: i64) -> anyhow::Result<RawStats> {
    let top_senders = config
        .accounts
        .iter()
        .map(|a| store.top_senders(since, TOP_SENDERS, Some(&a.name)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RawStats {
        categories: store.category_counts(since)?,
        flags: store.flag_counts(since)?,
        retries: store.retry_queue_summary(now)?,
        range: store.processed_range(since)?,
        top_senders_total: store.top_senders(since, TOP_SENDERS, None)?,
        top_senders,
    })
}

/// Current time as Unix seconds.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The start of the period: `days` days before `now`, never below zero.
fn since(now: i64, days: u32) -> i64 {
    (now - i64::from(days) * SECONDS_PER_DAY).max(0)
}

/// Shapes the raw rows into a [`Report`]: one entry per configured account plus a total.
///
/// The total is computed from all rows, so it also includes mail of accounts that have
/// since been removed from the config.
fn build_report(days: u32, dry_run: bool, accounts: &[&str], raw: RawStats) -> Report {
    let per_account: Vec<AccountStats> = accounts
        .iter()
        .zip(raw.top_senders)
        .map(|(name, top)| {
            let categories = counts_for(&raw.categories, Some(name));
            AccountStats {
                account: (*name).to_owned(),
                messages: categories.iter().map(|c| c.count).sum(),
                categories,
                flags: counts_for(&raw.flags, Some(name)),
                top_senders: top.into_iter().map(Count::from).collect(),
                retry_queued: raw
                    .retries
                    .iter()
                    .filter(|r| r.account == *name)
                    .map(|r| r.queued)
                    .sum(),
                retry_due: raw
                    .retries
                    .iter()
                    .filter(|r| r.account == *name)
                    .map(|r| r.due)
                    .sum(),
            }
        })
        .collect();
    let categories = counts_for(&raw.categories, None);
    let total = AccountStats {
        account: "total".to_owned(),
        messages: categories.iter().map(|c| c.count).sum(),
        categories,
        flags: counts_for(&raw.flags, None),
        top_senders: raw.top_senders_total.into_iter().map(Count::from).collect(),
        retry_queued: raw.retries.iter().map(|r| r.queued).sum(),
        retry_due: raw.retries.iter().map(|r| r.due).sum(),
    };
    Report {
        days,
        dry_run,
        first_day: raw.range.map(|(first, _)| format_day(first)),
        last_day: raw.range.map(|(_, last)| format_day(last)),
        accounts: per_account,
        total,
    }
}

impl From<Counted> for Count {
    fn from(c: Counted) -> Count {
        Count {
            name: c.label,
            count: c.count,
        }
    }
}

/// Sums the rows of one account (or of all accounts, with `None`) by label, biggest first.
fn counts_for(rows: &[AccountCount], account: Option<&str>) -> Vec<Count> {
    // A `BTreeMap` merges equal labels and iterates in alphabetical order, which makes the
    // tie-break in the sort below deterministic.
    let mut sums: BTreeMap<&str, u64> = BTreeMap::new();
    for row in rows
        .iter()
        .filter(|r| account.is_none_or(|a| r.account == a))
    {
        *sums.entry(&row.label).or_default() += row.count;
    }
    let mut counts: Vec<Count> = sums
        .into_iter()
        .map(|(name, count)| Count {
            name: name.to_owned(),
            count,
        })
        .collect();
    // `Reverse` flips the order (biggest first). The sort is stable: ties stay alphabetical.
    counts.sort_by_key(|c| std::cmp::Reverse(c.count));
    counts
}

/// Renders the report as plain text.
fn format_report(report: &Report) -> String {
    let mut out = String::new();
    out.push_str(&if report.dry_run {
        "Mode: dry-run. No flags are set on the server; the flag counts show what would be set.\n"
            .to_owned()
    } else {
        "Mode: live. Flags are set on the server.\n".to_owned()
    });
    out.push_str(&match (&report.first_day, &report.last_day) {
        (Some(first), Some(last)) => format!(
            "Last {} days. Messages logged from {first} to {last} (UTC).\n",
            report.days
        ),
        _ => format!(
            "Last {} days. No messages were logged in this period.\n",
            report.days
        ),
    });
    for stats in report.accounts.iter().chain(std::iter::once(&report.total)) {
        out.push('\n');
        out.push_str(&format_account(stats));
    }
    out.trim_end().to_owned()
}

/// Renders one account's block.
fn format_account(stats: &AccountStats) -> String {
    let mut out = format!("== {} ({} messages) ==\n", stats.account, stats.messages);
    out.push_str(&format_counts("Categories", &stats.categories));
    out.push_str(&format_counts("Flags", &stats.flags));
    out.push_str(&format_counts("Top senders", &stats.top_senders));
    out.push_str(&format!(
        "Model failures: {} in the retry queue, {} due now\n",
        stats.retry_queued, stats.retry_due
    ));
    out
}

/// Renders a titled list of counts, with the numbers right-aligned.
fn format_counts(title: &str, counts: &[Count]) -> String {
    if counts.is_empty() {
        return format!("{title}: none\n");
    }
    let width = counts
        .iter()
        .map(|c| c.count.to_string().len())
        .max()
        .unwrap_or(1);
    let mut out = format!("{title}:\n");
    for c in counts {
        out.push_str(&format!("  {:>width$}  {}\n", c.count, c.name));
    }
    out
}

/// Formats Unix seconds as a UTC date, `YYYY-MM-DD`. There is no date crate in the
/// project, so this converts days since 1970 to a calendar date by hand.
fn format_day(unix_seconds: i64) -> String {
    // Days since 1970-01-01 -> civil date: Howard Hinnant's `civil_from_days` algorithm.
    // It counts in 400-year eras (146097 days each), which have no irregular leap rules.
    let z = unix_seconds.div_euclid(SECONDS_PER_DAY) + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153; // month, counted from March
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn raw() -> RawStats {
        RawStats {
            categories: vec![
                ac("work", "personal", 3),
                ac("work", "newsletter", 1),
                ac("home", "newsletter", 4),
            ],
            flags: vec![ac("work", "reply_needed", 2), ac("home", "reply_needed", 1)],
            retries: vec![RetrySummary {
                account: "home".into(),
                queued: 2,
                due: 1,
            }],
            range: Some((1_761_350_400, 1_761_436_800)), // 2025-10-25 and 2025-10-26
            top_senders_total: vec![counted("a@x.com", 5)],
            top_senders: vec![vec![counted("a@x.com", 3)], vec![]],
        }
    }

    #[test]
    fn formats_known_dates() {
        assert_eq!(format_day(0), "1970-01-01");
        assert_eq!(format_day(951_782_400), "2000-02-29"); // a leap day
        assert_eq!(format_day(1_761_350_400), "2025-10-25");
        assert_eq!(format_day(-1), "1969-12-31");
    }

    #[test]
    fn since_goes_back_whole_days_and_stops_at_zero() {
        assert_eq!(since(1_000_000, 2), 1_000_000 - 172_800);
        assert_eq!(since(100, 7), 0);
    }

    #[test]
    fn report_sums_per_account_and_in_total() {
        let report = build_report(7, true, &["work", "home"], raw());
        let work = &report.accounts[0];
        assert_eq!(work.messages, 4);
        assert_eq!(
            work.categories[0],
            Count {
                name: "personal".into(),
                count: 3
            }
        );
        assert_eq!((work.retry_queued, work.retry_due), (0, 0));
        let home = &report.accounts[1];
        assert_eq!((home.retry_queued, home.retry_due), (2, 1));

        let total = &report.total;
        assert_eq!(total.messages, 8);
        // "newsletter" (1 + 4) overtakes "personal" (3) once the accounts are merged.
        assert_eq!(
            total.categories[0],
            Count {
                name: "newsletter".into(),
                count: 5
            }
        );
        assert_eq!(
            total.flags,
            vec![Count {
                name: "reply_needed".into(),
                count: 3
            }]
        );
        assert_eq!(report.first_day.as_deref(), Some("2025-10-25"));
        assert_eq!(report.last_day.as_deref(), Some("2025-10-26"));
    }

    #[test]
    fn text_output_names_the_mode_and_every_section() {
        let text = format_report(&build_report(7, true, &["work", "home"], raw()));
        assert!(text.contains("dry-run"), "{text}");
        assert!(text.contains("== work (4 messages) =="));
        assert!(text.contains("== total (8 messages) =="));
        assert!(text.contains("2 in the retry queue, 1 due now"));
        assert!(
            text.contains("Top senders: none"),
            "home has no senders in the sample"
        );

        let live = format_report(&build_report(7, false, &["work", "home"], raw()));
        assert!(live.contains("Mode: live"));
    }

    #[test]
    fn empty_database_still_prints() {
        let empty = RawStats {
            categories: vec![],
            flags: vec![],
            retries: vec![],
            range: None,
            top_senders_total: vec![],
            top_senders: vec![vec![]],
        };
        let text = format_report(&build_report(3, true, &["work"], empty));
        assert!(text.contains("No messages were logged"));
        assert!(text.contains("Categories: none"));
    }

    #[test]
    fn json_output_is_valid_and_has_the_counts() {
        let report = build_report(7, false, &["work", "home"], raw());
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string_pretty(&report).unwrap()).unwrap();
        assert_eq!(value["dry_run"], false);
        assert_eq!(value["total"]["messages"], 8);
        assert_eq!(value["accounts"][1]["retry_due"], 1);
    }

    #[test]
    fn query_reads_a_real_store() {
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
        assert!(raw.categories.is_empty() && raw.range.is_none());
        assert_eq!(raw.top_senders.len(), 1);
    }
}
