//! The `rules` command: list, add and remove sender allow and block rules.
//!
//! Rules are stored per account, or globally (account `*`). The command functions only do
//! IO; the pattern checking and the table layout are pure functions with their own tests.

use std::path::Path;

use anyhow::{Context, bail};
use clap::Subcommand;
use lets_triage_mail::config::Config;
use lets_triage_mail::engine::Rule;
use lets_triage_mail::store::{GLOBAL_ACCOUNT, RuleEntry, Store};

/// What to do with the sender rules.
#[derive(Debug, Subcommand)]
pub enum RulesCommand {
    /// List all rules (with --account: that account's rules and the global ones)
    List {
        /// Only rules that apply to this account (by name)
        #[arg(long)]
        account: Option<String>,
    },
    /// Always treat senders matching PATTERN as known
    Allow {
        /// An address or a domain such as `@example.com`
        pattern: String,
        /// Apply the rule to this account only (default: all accounts)
        #[arg(long)]
        account: Option<String>,
    },
    /// Never flag senders matching PATTERN as important
    Block {
        /// An address or a domain such as `@example.com`
        pattern: String,
        /// Apply the rule to this account only (default: all accounts)
        #[arg(long)]
        account: Option<String>,
    },
    /// Remove the rule for PATTERN
    Remove {
        /// The pattern exactly as it was added
        pattern: String,
        /// Remove the rule of this account (default: the global rule)
        #[arg(long)]
        account: Option<String>,
    },
}

/// Lists, adds or removes sender rules.
pub async fn run(config_path: &Path, command: RulesCommand) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;
    let db_path = config.db_path();
    // `Store` is synchronous (SQLite blocks the thread), so it runs on tokio's blocking
    // thread pool instead of the async runtime. The first `?` handles a panicked or
    // cancelled task, the second the `anyhow::Result` the closure itself returns.
    let output = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let store = Store::open(&db_path)
            .with_context(|| format!("cannot open the database {}", db_path.display()))?;
        execute(&config, &store, command)
    })
    .await??;
    println!("{output}");
    Ok(())
}

/// Carries out one command against the store and returns the text to print.
fn execute(config: &Config, store: &Store, command: RulesCommand) -> anyhow::Result<String> {
    match command {
        RulesCommand::List { account } => {
            let scope = account_scope(config, account.as_deref())?;
            let mut rules = store.list_rules()?;
            // A named account sees its own rules plus the global ones, which also apply to it.
            if account.is_some() {
                rules.retain(|r| r.account == scope || r.account == GLOBAL_ACCOUNT);
            }
            Ok(format_rules(&rules))
        }
        RulesCommand::Allow { pattern, account } => {
            add(config, store, &pattern, account, Rule::Allow)
        }
        RulesCommand::Block { pattern, account } => {
            add(config, store, &pattern, account, Rule::Block)
        }
        RulesCommand::Remove { pattern, account } => {
            let scope = account_scope(config, account.as_deref())?;
            let pattern = validate_pattern(&pattern)?;
            let removed = store.remove_rule(&scope, &pattern)?;
            Ok(format_removed(&scope, &pattern, removed))
        }
    }
}

/// Adds an allow or block rule and says what was stored.
fn add(
    config: &Config,
    store: &Store,
    pattern: &str,
    account: Option<String>,
    rule: Rule,
) -> anyhow::Result<String> {
    let scope = account_scope(config, account.as_deref())?;
    let pattern = validate_pattern(pattern)?;
    store.add_rule(&scope, &pattern, rule)?;
    Ok(format!(
        "{} {pattern} ({})",
        match rule {
            Rule::Allow => "Allowing",
            Rule::Block => "Blocking",
        },
        describe_scope(&scope)
    ))
}

/// The account a rule belongs to: the named one (which must exist in the config) or global.
fn account_scope(config: &Config, account: Option<&str>) -> anyhow::Result<String> {
    match account {
        None => Ok(GLOBAL_ACCOUNT.to_owned()),
        Some(name) if config.account(name).is_some() => Ok(name.to_owned()),
        Some(name) => {
            let known: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
            bail!(
                "no account named `{name}` in the config (accounts: {})",
                known.join(", ")
            )
        }
    }
}

/// Checks and normalises a rule pattern: `name@example.com` or `@example.com`, lowercase.
///
/// The store only checks for an `@`; the command is stricter so that typos such as
/// `example.com` or `a@b@c` fail loudly instead of becoming rules that never match.
fn validate_pattern(input: &str) -> anyhow::Result<String> {
    let pattern = input.trim().to_lowercase();
    let invalid = || {
        anyhow::anyhow!(
            "`{input}` is not a valid pattern: use an address (name@example.com) \
             or a domain (@example.com)"
        )
    };
    // `split_once` splits at the first `@`; any second `@` ends up in `domain`.
    let (local, domain) = pattern.split_once('@').ok_or_else(invalid)?;
    let domain_ok = !domain.is_empty()
        && !domain.contains('@')
        && !domain.starts_with(['.', '-'])
        && !domain.ends_with(['.', '-'])
        && !domain.contains("..")
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || !c.is_ascii());
    let local_ok = local.chars().all(|c| !c.is_whitespace() && !c.is_control());
    if domain_ok && local_ok {
        Ok(pattern)
    } else {
        Err(invalid())
    }
}

/// Human wording for a rule's account.
fn describe_scope(scope: &str) -> String {
    if scope == GLOBAL_ACCOUNT {
        "all accounts".to_owned()
    } else {
        format!("account {scope}")
    }
}

/// The message after `rules remove`.
fn format_removed(scope: &str, pattern: &str, removed: bool) -> String {
    if removed {
        format!("Removed the rule for {pattern} ({})", describe_scope(scope))
    } else {
        format!("No such rule: {pattern} ({})", describe_scope(scope))
    }
}

/// Lays the rules out as a table with the columns ACCOUNT, PATTERN and RULE.
fn format_rules(rules: &[RuleEntry]) -> String {
    if rules.is_empty() {
        return "No rules.".to_owned();
    }
    let account_width = rules
        .iter()
        .map(|r| r.account.chars().count())
        .max()
        .unwrap_or(0)
        .max("ACCOUNT".len());
    let pattern_width = rules
        .iter()
        .map(|r| r.pattern.chars().count())
        .max()
        .unwrap_or(0)
        .max("PATTERN".len());
    let mut lines = vec![format!(
        "{:<account_width$}  {:<pattern_width$}  RULE",
        "ACCOUNT", "PATTERN"
    )];
    for r in rules {
        let kind = match r.rule {
            Rule::Allow => "allow",
            Rule::Block => "block",
        };
        lines.push(format!(
            "{:<account_width$}  {:<pattern_width$}  {kind}",
            r.account, r.pattern
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::parse(
            r#"
            [[accounts]]
            name = "work"
            host = "imap.example.com"
            username = "me@example.com"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn patterns_are_lowercased_and_trimmed() {
        assert_eq!(
            validate_pattern(" Bob@Example.COM ").unwrap(),
            "bob@example.com"
        );
        assert_eq!(validate_pattern("@Example.com").unwrap(), "@example.com");
    }

    #[test]
    fn bad_patterns_are_rejected_with_a_hint() {
        for bad in [
            "",
            "@",
            "example.com",
            "a@",
            "a@b@c.com",
            "@exa mple.com",
            "a@.com",
            "a@b..com",
            "a b@c.com",
        ] {
            let err = validate_pattern(bad).unwrap_err().to_string();
            assert!(err.contains("@example.com"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn unknown_account_is_an_error_and_none_is_global() {
        let config = config();
        assert_eq!(account_scope(&config, None).unwrap(), "*");
        assert_eq!(account_scope(&config, Some("work")).unwrap(), "work");
        let err = account_scope(&config, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope") && err.contains("work"), "{err}");
    }

    #[test]
    fn empty_table_says_so() {
        assert_eq!(format_rules(&[]), "No rules.");
    }

    #[test]
    fn table_lines_up_columns() {
        let rules = [
            RuleEntry {
                account: "*".into(),
                pattern: "@spam.example".into(),
                rule: Rule::Block,
            },
            RuleEntry {
                account: "work".into(),
                pattern: "boss@example.com".into(),
                rule: Rule::Allow,
            },
        ];
        let expected = "\
ACCOUNT  PATTERN           RULE
*        @spam.example     block
work     boss@example.com  allow";
        assert_eq!(format_rules(&rules), expected);
    }

    #[test]
    fn removal_message_depends_on_the_outcome() {
        assert!(format_removed("*", "@a.com", true).starts_with("Removed"));
        assert_eq!(
            format_removed("work", "@a.com", false),
            "No such rule: @a.com (account work)"
        );
    }

    #[test]
    fn add_list_and_remove_round_trip() {
        let config = config();
        let store = Store::open_in_memory().unwrap();
        let run = |command| execute(&config, &store, command).unwrap();

        run(RulesCommand::Block {
            pattern: "@Spam.example".into(),
            account: None,
        });
        run(RulesCommand::Allow {
            pattern: "boss@example.com".into(),
            account: Some("work".into()),
        });

        let all = run(RulesCommand::List { account: None });
        assert!(all.contains("@spam.example") && all.contains("boss@example.com"));

        let removed = run(RulesCommand::Remove {
            pattern: "@SPAM.example".into(),
            account: None,
        });
        assert!(removed.starts_with("Removed"));
        let again = run(RulesCommand::Remove {
            pattern: "@spam.example".into(),
            account: None,
        });
        assert!(again.starts_with("No such rule"));
    }

    #[test]
    fn listing_one_account_includes_global_rules_only() {
        let mut config = config();
        config.accounts.push(config.accounts[0].clone());
        config.accounts[1].name = "home".into();
        let store = Store::open_in_memory().unwrap();
        store.add_rule("*", "@global.example", Rule::Block).unwrap();
        store
            .add_rule("work", "a@work.example", Rule::Allow)
            .unwrap();
        store
            .add_rule("home", "b@home.example", Rule::Allow)
            .unwrap();

        let out = execute(
            &config,
            &store,
            RulesCommand::List {
                account: Some("work".into()),
            },
        )
        .unwrap();
        assert!(out.contains("@global.example") && out.contains("a@work.example"));
        assert!(!out.contains("b@home.example"));
    }
}
