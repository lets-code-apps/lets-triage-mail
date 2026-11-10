//! The `config` command: show and validate the config, and edit accounts without losing the
//! user's comments. The desktop app calls these with `--json`; the JSON shapes are locked by
//! the tests below. The edits themselves are pure functions in
//! [`lets_triage_mail::config_edit`]; this file only does the IO and the output.

use std::io::BufRead;
use std::path::Path;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use lets_triage_mail::config::Config;
use lets_triage_mail::config_edit::{self as edit, NewAccount};
use serde_json::{Value, json};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the config path and the settings a settings screen needs (never a password command)
    Show {
        /// Print machine-readable JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Parse and validate the config (no network, no password lookup)
    Validate {
        /// Print machine-readable JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Add, change or remove an account, keeping the file's comments
    #[command(subcommand)]
    Account(AccountCommand),
}

#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Add an account (creates the config file if there is none)
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long)]
        username: String,
        /// Default: 993 for tls, 143 otherwise
        #[arg(long)]
        port: Option<u16>,
        /// tls (default), starttls or none (loopback only)
        #[arg(long)]
        tls: Option<String>,
        /// true or false; default: follow `[policy] dry_run`
        #[arg(long)]
        dry_run: Option<bool>,
        /// false = classify for the digest only
        #[arg(long)]
        flagging: Option<bool>,
        /// Read the password from stdin (one line) and store it in the login Keychain
        #[arg(long)]
        password_stdin: bool,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Change one setting of an account
    Set {
        name: String,
        /// One of host, port, username, tls, inbox, flagging, watch_junk, dry_run
        key: String,
        /// For dry_run, `default` removes the key (follow the policy)
        value: String,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Remove an account from the config
    Remove {
        name: String,
        /// Also delete its Keychain item (only if the config points at this tool's item)
        #[arg(long)]
        forget_password: bool,
        #[command(flatten)]
        out: JsonFlag,
    },
    /// Replace an account's Keychain password
    Password {
        name: String,
        /// Read the password from stdin (one line)
        #[arg(long, required = true)]
        password_stdin: bool,
        #[command(flatten)]
        out: JsonFlag,
    },
}

#[derive(Debug, Args)]
pub struct JsonFlag {
    /// Print machine-readable JSON instead of text
    #[arg(long)]
    json: bool,
}

/// The config file does not exist; `show --json` reports it with `"missing": true`.
#[derive(Debug, thiserror::Error)]
#[error("no config file at {0}")]
struct MissingConfig(String);

/// What a command prints: the JSON for the app and the short text for people.
struct Outcome {
    json: Value,
    text: String,
}

impl ConfigCommand {
    fn wants_json(&self) -> bool {
        match self {
            ConfigCommand::Show { json } | ConfigCommand::Validate { json } => *json,
            ConfigCommand::Account(
                AccountCommand::Add { out, .. }
                | AccountCommand::Set { out, .. }
                | AccountCommand::Remove { out, .. }
                | AccountCommand::Password { out, .. },
            ) => out.json,
        }
    }
}

/// Runs one config command. In JSON mode a failure is `{"ok":false,...}` on stdout and exit 1.
pub async fn run(config_path: &Path, command: ConfigCommand) -> anyhow::Result<()> {
    let json = command.wants_json();
    match execute(config_path, command) {
        Ok(outcome) if json => println!("{}", serde_json::to_string_pretty(&outcome.json)?),
        Ok(outcome) => println!("{}", outcome.text),
        Err(error) if json => {
            println!("{}", failure_json(&error));
            std::process::exit(1);
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn failure_json(error: &anyhow::Error) -> Value {
    let mut value = json!({"ok": false, "error": format!("{error:#}")});
    if error.downcast_ref::<MissingConfig>().is_some() {
        value["missing"] = json!(true);
    }
    value
}

fn execute(path: &Path, command: ConfigCommand) -> anyhow::Result<Outcome> {
    match command {
        ConfigCommand::Show { .. } => show(path),
        ConfigCommand::Validate { .. } => {
            let config = load(path)?;
            Ok(Outcome {
                json: json!({"ok": true, "path": path, "accounts": config.accounts.len()}),
                text: format!("{} is valid", path.display()),
            })
        }
        ConfigCommand::Account(AccountCommand::Add {
            name,
            host,
            username,
            port,
            tls,
            dry_run,
            flagging,
            password_stdin,
            ..
        }) => add(
            path,
            NewAccount {
                name,
                host,
                username,
                port,
                tls,
                dry_run,
                flagging,
                keychain_password: password_stdin,
            },
        ),
        ConfigCommand::Account(AccountCommand::Set {
            name, key, value, ..
        }) => {
            let new = edit::set_account_key(&read(path)?, &name, &key, &value)?;
            commit(path, &new)?;
            Ok(changed(format!("{name}: {key} updated")))
        }
        ConfigCommand::Account(AccountCommand::Remove {
            name,
            forget_password,
            ..
        }) => remove(path, &name, forget_password),
        ConfigCommand::Account(AccountCommand::Password { name, .. }) => password(path, &name),
    }
}

fn read(path: &Path) -> anyhow::Result<String> {
    if !path.exists() {
        return Err(MissingConfig(path.display().to_string()).into());
    }
    std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))
}

fn load(path: &Path) -> anyhow::Result<Config> {
    read(path)?;
    Ok(Config::load(path)?)
}

/// Validates `new_text` like a normal start, then writes it. Nothing is written if invalid.
fn commit(path: &Path, new_text: &str) -> anyhow::Result<Config> {
    let config = edit::validate_text(new_text)?;
    edit::write_config(path, new_text)?;
    Ok(config)
}

fn changed(text: String) -> Outcome {
    Outcome {
        json: json!({"ok": true, "restart_needed": true}),
        text: format!("{text}; restart triage to apply"),
    }
}

/// Reads the password: one line from stdin, without the trailing newline.
fn read_password() -> anyhow::Result<String> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let password = line.trim_end_matches(['\n', '\r']).to_owned();
    edit::check_password(&password)?;
    Ok(password)
}

fn add(path: &Path, account: NewAccount) -> anyhow::Result<Outcome> {
    let password = account.keychain_password.then(read_password).transpose()?;
    let text = if path.exists() {
        read(path)?
    } else {
        String::new()
    };
    let new = edit::add_account(&text, &account)?;
    // Validate first, so a rejected account (say a duplicate name) never touches the Keychain.
    edit::validate_text(&new)?;
    if let Some(password) = &password {
        edit::store_password(&account.name, password)?;
    }
    commit(path, &new).map_err(|e| {
        if password.is_some() {
            e.context("the password is in the Keychain but the config was not written")
        } else {
            e
        }
    })?;
    let hint = if password.is_some() {
        String::new()
    } else {
        format!(
            "; no password set: export {} or run `config account password`",
            lets_triage_mail::config::password_env_var(&account.name)
        )
    };
    let mut outcome = changed(format!("added account {}", account.name));
    outcome.text.push_str(&hint);
    Ok(outcome)
}

fn remove(path: &Path, name: &str, forget: bool) -> anyhow::Result<Outcome> {
    let text = read(path)?;
    let before = edit::validate_text(&text)?;
    let ours = before
        .account(name)
        .is_some_and(|a| edit::password_source(a) == "keychain");
    commit(path, &edit::remove_account(&text, name)?)?;
    let forgotten = forget && ours && edit::forget_password(name)?;
    let note = match (forget, ours, forgotten) {
        (false, ..) => "",
        (true, false, _) => "; left the password alone (not stored by this tool)",
        (true, true, false) => "; no Keychain item was found",
        (true, true, true) => "; Keychain item deleted",
    };
    let mut outcome = changed(format!("removed account {name}{note}"));
    outcome.json["password_forgotten"] = json!(forgotten);
    Ok(outcome)
}

fn password(path: &Path, name: &str) -> anyhow::Result<Outcome> {
    let text = read(path)?;
    if edit::validate_text(&text)?.account(name).is_none() {
        bail!("no account named `{name}` in the config");
    }
    let password = read_password()?;
    edit::store_password(name, &password)?;
    let new = edit::ensure_keychain_command(&text, name)?;
    let file_changed = new != text;
    if file_changed {
        commit(path, &new)?;
    }
    Ok(Outcome {
        json: json!({"ok": true, "restart_needed": file_changed}),
        text: format!("password for {name} stored in the Keychain; restart triage to apply"),
    })
}

fn show(path: &Path) -> anyhow::Result<Outcome> {
    let config = load(path)?;
    let json = show_json(path, &config);
    let mut text = format!("{}\ndry_run {}\n", path.display(), config.policy.dry_run);
    text += &format!("model {} at {}\n", config.llm.model, config.llm.base_url);
    for a in json["accounts"].as_array().into_iter().flatten() {
        text += &format!(
            "{}: {}@{}:{} ({}), password from {}\n",
            a["name"].as_str().unwrap_or_default(),
            a["username"].as_str().unwrap_or_default(),
            a["host"].as_str().unwrap_or_default(),
            a["port"],
            a["mode"].as_str().unwrap_or_default(),
            a["password_source"].as_str().unwrap_or_default(),
        );
    }
    Ok(Outcome {
        json,
        text: text.trim_end().to_owned(),
    })
}

/// The shape of `config show --json`. The desktop app parses it, so a test locks it.
fn show_json(path: &Path, config: &Config) -> Value {
    let accounts: Vec<Value> = config
        .accounts
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "host": a.host,
                "port": a.port(),
                "username": a.username,
                "tls": edit::tls_name(a.tls),
                "inbox": a.inbox,
                "flagging": a.flagging,
                "watch_junk": a.watch_junk,
                "dry_run": a.dry_run(&config.policy),
                "mode": a.flag_mode(&config.policy).as_str(),
                "password_source": edit::password_source(a),
            })
        })
        .collect();
    json!({
        "ok": true,
        "path": path,
        "dry_run": config.policy.dry_run,
        "llm": {"base_url": config.llm.base_url, "model": config.llm.model},
        "accounts": accounts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_json_shape_is_locked() {
        let config = Config::parse(
            r#"
            [[accounts]]
            name = "work"
            host = "imap.example.com"
            username = "me@example.com"
            password_command = "echo hunter2"
            flagging = false
            "#,
        )
        .unwrap();
        let value = show_json(Path::new("/x/config.toml"), &config);
        assert_eq!(
            value,
            json!({
                "ok": true,
                "path": "/x/config.toml",
                "dry_run": true,
                "llm": {"base_url": "http://127.0.0.1:11434/v1", "model": "qwen3:14b"},
                "accounts": [{
                    "name": "work",
                    "host": "imap.example.com",
                    "port": 993,
                    "username": "me@example.com",
                    "tls": "tls",
                    "inbox": "INBOX",
                    "flagging": false,
                    "watch_junk": true,
                    "dry_run": true,
                    "mode": "classify-only",
                    "password_source": "command",
                }],
            })
        );
        // The command text itself never appears.
        assert!(!value.to_string().contains("hunter2"));
    }

    #[test]
    fn failure_json_shapes_are_locked() {
        assert_eq!(
            failure_json(&anyhow::anyhow!("boom")),
            json!({"ok": false, "error": "boom"})
        );
        assert_eq!(
            failure_json(&MissingConfig("/x".into()).into()),
            json!({"ok": false, "error": "no config file at /x", "missing": true})
        );
    }

    #[test]
    fn changed_outcome_tells_the_app_to_restart() {
        let o = changed("did it".into());
        assert_eq!(o.json, json!({"ok": true, "restart_needed": true}));
        assert_eq!(o.text, "did it; restart triage to apply");
    }

    #[test]
    fn missing_file_is_reported_as_missing() {
        let err = show(Path::new("/nonexistent/c.toml")).err().unwrap();
        assert!(err.downcast_ref::<MissingConfig>().is_some());
    }

    #[test]
    fn full_edit_cycle_on_a_scratch_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c/config.toml");
        let account = |name: &str| NewAccount {
            name: name.into(),
            host: "imap.example.com".into(),
            username: "me".into(),
            ..Default::default()
        };
        add(&path, account("work")).unwrap();
        add(&path, account("home")).unwrap();
        // A duplicate is refused and the file is untouched.
        let before = std::fs::read_to_string(&path).unwrap();
        assert!(add(&path, account("work")).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        execute(
            &path,
            ConfigCommand::Account(AccountCommand::Set {
                name: "home".into(),
                key: "port".into(),
                value: "1143".into(),
                out: JsonFlag { json: true },
            }),
        )
        .unwrap();
        assert_eq!(load(&path).unwrap().account("home").unwrap().port(), 1143);
        remove(&path, "work", false).unwrap();
        assert_eq!(load(&path).unwrap().accounts.len(), 1);
        remove(&path, "home", false).unwrap();
        assert!(load(&path).unwrap().accounts.is_empty());
        assert!(path.with_extension("toml.bak").exists());
    }
}
