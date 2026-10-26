//! The `digest` command: build a digest now.
//!
//! - `--dry` prints the plain-text digest and changes nothing: no `APPEND`, no preview file and
//!   no update of the "last digest" record, so the scheduled digest is not affected.
//! - Without `--dry` the digest is delivered the way the schedule would deliver it: appended to
//!   the mailbox, or, for an account in dry-run, saved as a file under the data directory (the
//!   mailbox is never written to in dry-run). The day is recorded, so the scheduled digest of
//!   today is then not sent a second time.
//!
//! The digest covers the period since the previous one (at most two days), so running this
//! twice in a row shows an almost empty second digest.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use lets_triage_mail::digest::deliver::{Delivery, prepare_digest, send_digest};
use lets_triage_mail::store::Store;

/// Builds a digest immediately. With `dry` it is printed; otherwise it is delivered.
/// `account` limits the run to one configured account.
///
/// A failing account does not stop the others; the command then exits non-zero.
pub async fn run(config_path: &Path, account: Option<&str>, dry: bool) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;
    let accounts = super::check::select_accounts(&config, account)?;
    if accounts.is_empty() {
        anyhow::bail!("the config has no accounts");
    }
    let store = Arc::new(Mutex::new(
        Store::open(&config.db_path()).context("could not open the state database")?,
    ));
    let now = jiff::Zoned::now();

    let mut failed = Vec::new();
    for (index, account) in accounts.iter().enumerate() {
        let result = if dry {
            prepare_digest(account, &config, &store, &now)
                .await
                .map(|prepared| {
                    if accounts.len() > 1 {
                        if index > 0 {
                            println!();
                        }
                        println!("===== {} =====", account.name);
                    }
                    print!("{}", prepared.rendered.text);
                })
        } else {
            send_digest(account, &config, &store, &now)
                .await
                .map(|delivery| println!("{}: {}", account.name, describe(&delivery)))
        };
        if let Err(error) = result {
            eprintln!("{}: digest failed: {error}", account.name);
            failed.push(account.name.clone());
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("digest failed for: {}", failed.join(", "))
    }
}

/// What happened, in plain words.
fn describe(delivery: &Delivery) -> String {
    match delivery {
        Delivery::Appended {
            folder,
            uid: Some(uid),
        } => {
            format!("digest appended to {folder} (UID {uid}), unread")
        }
        Delivery::Appended { folder, uid: None } => {
            format!("digest appended to {folder}, unread")
        }
        Delivery::Saved(path) => format!(
            "dry-run account: nothing was appended; the digest was saved to {}",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_result_is_described_in_plain_words() {
        let appended = Delivery::Appended {
            folder: "INBOX".into(),
            uid: Some(7),
        };
        assert_eq!(
            describe(&appended),
            "digest appended to INBOX (UID 7), unread"
        );
        let saved = Delivery::Saved(PathBuf::from("/d/work-2026-10-26.eml"));
        assert!(describe(&saved).contains("nothing was appended"));
        assert!(describe(&saved).contains("/d/work-2026-10-26.eml"));
    }
}
