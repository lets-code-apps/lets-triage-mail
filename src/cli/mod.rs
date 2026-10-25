//! The command-line interface: the argument types (parsed by `clap`) and the dispatch.
//!
//! Each subcommand lives in its own file and exposes one `async fn run` that returns
//! `anyhow::Result<()>`. Every command is still a stub; see `docs/PLAN.md`.

mod backfill;
mod check;
mod classify;
mod digest;
mod rules;
mod run;
mod stats;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{ArgAction, Parser, Subcommand};
use lets_triage_mail::config::Config;

/// Where the config lives when `--config` is not given (see `docs/SPEC.md` section 3).
const DEFAULT_CONFIG_PATH: &str = "~/.config/lets-triage-mail/config.toml";

// `#[derive(Parser)]` is a macro from `clap`: it writes the argument parsing code for us
// from this struct. The doc comments on the fields become the `--help` text; the program
// description comes from `Cargo.toml` through `about`. (Plain comments here on purpose: a
// doc comment on the struct would replace that description.)
#[derive(Debug, Parser)]
#[command(name = "lets-triage-mail", version, about)]
pub struct Cli {
    /// Path to the config file [default: ~/.config/lets-triage-mail/config.toml]
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// More logging: -v for debug, -vv for trace (RUST_LOG overrides)
    #[arg(short, long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// The config file to use: `--config` if given, otherwise the default location.
    pub fn config_path(&self) -> PathBuf {
        let path = match &self.config {
            Some(path) => path.clone(),
            None => PathBuf::from(DEFAULT_CONFIG_PATH),
        };
        expand_home(path, std::env::var_os("HOME"))
    }
}

/// The top-level subcommands, as listed in `docs/SPEC.md` section 4.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Watch the accounts, flag new mail and send the daily digest
    Run,
    /// Test the model and each account; show server capabilities and folders
    Check,
    /// Classify one message without touching any mailbox
    Classify {
        /// One or more .eml files (for example exported from Apple Mail)
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
        /// Print one JSON object per file instead of readable text
        #[arg(long)]
        json: bool,
    },
    /// Classify and flag existing inbox mail from the last few days
    Backfill {
        /// How many days back to look
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Only this account (by name)
        #[arg(long)]
        account: Option<String>,
    },
    /// Build and print or append a digest immediately
    Digest {
        /// Only this account (by name)
        #[arg(long)]
        account: Option<String>,
        /// Print the digest instead of appending it to the mailbox
        #[arg(long)]
        dry: bool,
    },
    /// Show what the service has done recently
    Stats {
        /// How many days back to cover
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Print machine-readable JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Manage the sender allow and block rules
    #[command(subcommand)]
    Rules(rules::RulesCommand),
}

/// Runs the chosen subcommand.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let config_path = cli.config_path();
    tracing::debug!(config = %config_path.display(), "using config file");
    match cli.command {
        Command::Run => run::run(&config_path).await,
        Command::Check => check::run(&config_path).await,
        Command::Classify { files, json } => classify::run(&config_path, &files, json).await,
        Command::Backfill { days, account } => {
            backfill::run(&config_path, days, account.as_deref()).await
        }
        Command::Digest { account, dry } => {
            digest::run(&config_path, account.as_deref(), dry).await
        }
        Command::Stats { days, json } => stats::run(&config_path, days, json).await,
        Command::Rules(command) => rules::run(&config_path, command).await,
    }
}

/// Loads the config for a command, with a pointer to the example file when it is missing.
fn load_config(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        anyhow::bail!(
            "config file {} not found; copy config.example.toml there and adjust it, \
             or pass --config PATH",
            path.display()
        );
    }
    Ok(Config::load(path)?)
}

/// Replaces a leading `~` with the home directory.
///
/// The shell does this for arguments typed on the command line, but not for our built-in
/// default, so we do it ourselves. `home` is passed in (instead of read here) so tests do not
/// have to change the process environment.
fn expand_home(path: PathBuf, home: Option<OsString>) -> PathBuf {
    match (path.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    /// Parses `args` as if they were typed after the program name.
    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("lets-triage-mail").chain(args.iter().copied()))
    }

    #[test]
    fn clap_definition_is_valid() {
        // Catches mistakes such as duplicate flags at test time instead of at runtime.
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_backfill_with_options() {
        let cli = parse(&["backfill", "--days", "3", "--account", "work"]).unwrap();
        match cli.command {
            Command::Backfill { days, account } => {
                assert_eq!(days, 3);
                assert_eq!(account.as_deref(), Some("work"));
            }
            other => panic!("expected backfill, got {other:?}"),
        }
    }

    #[test]
    fn backfill_and_stats_default_to_seven_days() {
        let cli = parse(&["backfill"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Backfill {
                days: 7,
                account: None
            }
        ));
        let cli = parse(&["stats"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Stats {
                days: 7,
                json: false
            }
        ));
    }

    #[test]
    fn parses_stats_options() {
        let cli = parse(&["stats", "--days", "30", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Stats {
                days: 30,
                json: true
            }
        ));
    }

    #[test]
    fn parses_classify_with_several_files_and_json() {
        let cli = parse(&["classify", "a.eml", "b.eml", "--json"]).unwrap();
        match cli.command {
            Command::Classify { files, json } => {
                assert_eq!(files, [PathBuf::from("a.eml"), PathBuf::from("b.eml")]);
                assert!(json);
            }
            other => panic!("expected classify, got {other:?}"),
        }
        assert!(matches!(
            parse(&["classify", "a.eml"]).unwrap().command,
            Command::Classify { json: false, .. }
        ));
    }

    #[test]
    fn classify_needs_at_least_one_file() {
        assert!(parse(&["classify"]).is_err());
    }

    #[test]
    fn parses_rules_allow_with_and_without_account() {
        let cli = parse(&["rules", "allow", "@example.com"]).unwrap();
        match cli.command {
            Command::Rules(rules::RulesCommand::Allow { pattern, account }) => {
                assert_eq!(pattern, "@example.com");
                assert_eq!(account, None);
            }
            other => panic!("expected rules allow, got {other:?}"),
        }
        let cli = parse(&["rules", "block", "a@b.com", "--account", "work"]).unwrap();
        match cli.command {
            Command::Rules(rules::RulesCommand::Block { pattern, account }) => {
                assert_eq!(pattern, "a@b.com");
                assert_eq!(account.as_deref(), Some("work"));
            }
            other => panic!("expected rules block, got {other:?}"),
        }
    }

    #[test]
    fn parses_rules_remove_and_list_account() {
        let cli = parse(&["rules", "remove", "@b.com", "--account", "home"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Rules(rules::RulesCommand::Remove {
                account: Some(_),
                ..
            })
        ));
        let cli = parse(&["rules", "list", "--account", "home"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Rules(rules::RulesCommand::List { account: Some(_) })
        ));
    }

    #[test]
    fn rules_list_takes_no_pattern() {
        assert!(parse(&["rules", "list"]).is_ok());
        assert!(parse(&["rules", "list", "x"]).is_err());
        assert!(parse(&["rules", "block"]).is_err());
    }

    #[test]
    fn missing_config_error_points_at_the_example() {
        let err = load_config(Path::new("/nonexistent/config.toml")).unwrap_err();
        assert!(err.to_string().contains("config.example.toml"), "{err}");
    }

    #[test]
    fn parses_digest_dry_run() {
        let cli = parse(&["digest", "--dry"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Digest {
                account: None,
                dry: true
            }
        ));
    }

    #[test]
    fn counts_verbosity_flags() {
        assert_eq!(parse(&["-vv", "check"]).unwrap().verbose, 2);
        // Global options also work after the subcommand.
        assert_eq!(parse(&["check", "-v"]).unwrap().verbose, 1);
        assert_eq!(parse(&["check"]).unwrap().verbose, 0);
    }

    #[test]
    fn default_config_path_expands_home() {
        let path = expand_home(PathBuf::from(DEFAULT_CONFIG_PATH), Some("/Users/me".into()));
        assert_eq!(
            path,
            PathBuf::from("/Users/me/.config/lets-triage-mail/config.toml")
        );
    }

    #[test]
    fn explicit_config_path_is_kept() {
        let cli = parse(&["--config", "/tmp/c.toml", "check"]).unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/c.toml")));
        assert_eq!(cli.config_path(), PathBuf::from("/tmp/c.toml"));
    }
}
