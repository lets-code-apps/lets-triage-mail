//! Editing the config file without losing the user's comments: the pure text edits, the one
//! write path, and the Keychain helpers behind `lets-triage-mail config ...`.
//!
//! Every edit is `text -> text` (via `toml_edit`, which keeps comments and layout), the result
//! is parsed and validated like a normal start would, and only then written. Passwords never
//! go in the file: only a `password_command` that reads the login Keychain.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use toml_edit::{DocumentMut, Item, Table, Value, value};

use crate::config::{AccountConfig, Config};

/// The Keychain service all passwords of this tool live under.
const KEYCHAIN_SERVICE: &str = "lets-triage-mail";

/// The Keychain service name. `LTM_KEYCHAIN_SERVICE` exists for hand checks only, so a test
/// never touches the real items.
pub fn keychain_service() -> String {
    std::env::var("LTM_KEYCHAIN_SERVICE").unwrap_or_else(|_| KEYCHAIN_SERVICE.to_owned())
}

/// Keys `config account set` may change.
pub const SETTABLE_KEYS: [&str; 8] = [
    "host",
    "port",
    "username",
    "tls",
    "inbox",
    "flagging",
    "watch_junk",
    "dry_run",
];

const HEADER: &str =
    "# Let's Triage Mail configuration. See config.example.toml for every option.\n";

/// The values `config account add` writes; only the ones given end up in the file.
#[derive(Debug, Default)]
pub struct NewAccount {
    pub name: String,
    pub host: String,
    pub username: String,
    pub port: Option<u16>,
    pub tls: Option<String>,
    pub dry_run: Option<bool>,
    pub flagging: Option<bool>,
    pub keychain_password: bool,
}

/// Account names end up inside shell and `security` quoting, so only quote-safe characters.
pub fn check_account_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " -_.@".contains(c));
    if !ok {
        bail!("account name `{name}` is not allowed: use only letters, digits, spaces and - _ . @");
    }
    Ok(())
}

/// The `password_command` this tool writes for an account.
pub fn keychain_password_command(service: &str, name: &str) -> String {
    format!("security find-generic-password -s {service} -a '{name}' -w")
}

/// Where an account's password comes from, for `config show`.
pub fn password_source(account: &AccountConfig) -> &'static str {
    let command = account.password_command.trim();
    if command.is_empty() {
        "env"
    } else if command == keychain_password_command(&keychain_service(), &account.name) {
        "keychain"
    } else {
        "command"
    }
}

/// The `tls` word for JSON and text.
pub fn tls_name(tls: crate::config::Tls) -> &'static str {
    match tls {
        crate::config::Tls::Tls => "tls",
        crate::config::Tls::Starttls => "starttls",
        crate::config::Tls::None => "none",
    }
}

fn account_tables(doc: &mut DocumentMut) -> Option<&mut toml_edit::ArrayOfTables> {
    doc.get_mut("accounts")?.as_array_of_tables_mut()
}

fn find_account(doc: &mut DocumentMut, name: &str) -> Result<usize> {
    let known: Vec<String> = account_tables(doc)
        .map(|tables| {
            tables
                .iter()
                .filter_map(|t| t.get("name")?.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    match known.iter().position(|n| n == name) {
        Some(index) => Ok(index),
        None => bail!(
            "no account named `{name}` in the config (accounts: {})",
            known.join(", ")
        ),
    }
}

fn parse_doc(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>()
        .map_err(|e| anyhow::anyhow!("the config file is not valid TOML:\n{e}"))
}

/// Appends an `[[accounts]]` table (after the last existing one) with the keys given.
pub fn add_account(text: &str, account: &NewAccount) -> Result<String> {
    check_account_name(&account.name)?;
    let mut doc = parse_doc(text)?;
    let mut table = Table::new();
    table["name"] = value(account.name.as_str());
    table["host"] = value(account.host.as_str());
    table["username"] = value(account.username.as_str());
    if let Some(port) = account.port {
        table["port"] = value(i64::from(port));
    }
    if let Some(tls) = &account.tls {
        table["tls"] = value(tls.as_str());
    }
    if let Some(flagging) = account.flagging {
        table["flagging"] = value(flagging);
    }
    if let Some(dry_run) = account.dry_run {
        table["dry_run"] = value(dry_run);
    }
    if account.keychain_password {
        table["password_command"] = value(keychain_password_command(
            &keychain_service(),
            &account.name,
        ));
    }
    match account_tables(&mut doc) {
        Some(tables) => tables.push(table),
        None => {
            let mut tables = toml_edit::ArrayOfTables::new();
            tables.push(table);
            doc.insert("accounts", Item::ArrayOfTables(tables));
        }
    }
    let out = doc.to_string();
    // A new file (empty text) starts with a short header comment.
    Ok(if text.is_empty() {
        format!("{HEADER}\n{out}")
    } else {
        out
    })
}

/// A typed TOML value for one of [`SETTABLE_KEYS`]; `None` means "remove the key".
fn typed_value(key: &str, raw: &str) -> Result<Option<Value>> {
    let bool_value = || -> Result<Value> {
        match raw {
            "true" => Ok(true.into()),
            "false" => Ok(false.into()),
            _ => bail!("`{key}` must be true or false, not `{raw}`"),
        }
    };
    Ok(Some(match key {
        "host" | "username" | "inbox" => raw.into(),
        "port" => {
            let port: u16 = raw.parse().map_err(|_| {
                anyhow::anyhow!("`port` must be a number from 0 to 65535, not `{raw}`")
            })?;
            i64::from(port).into()
        }
        "tls" if ["tls", "starttls", "none"].contains(&raw) => raw.into(),
        "tls" => bail!("`tls` must be tls, starttls or none, not `{raw}`"),
        "flagging" | "watch_junk" => bool_value()?,
        "dry_run" if raw == "default" => return Ok(None),
        "dry_run" => bool_value()?,
        _ => bail!(
            "`{key}` cannot be set here; allowed keys: {}",
            SETTABLE_KEYS.join(", ")
        ),
    }))
}

/// Sets (or, for `dry_run default`, removes) one key of one account.
pub fn set_account_key(text: &str, name: &str, key: &str, raw: &str) -> Result<String> {
    let new = typed_value(key, raw)?;
    set_account_value(text, name, key, new)
}

fn set_account_value(text: &str, name: &str, key: &str, new: Option<Value>) -> Result<String> {
    let mut doc = parse_doc(text)?;
    let index = find_account(&mut doc, name)?;
    let table = account_tables(&mut doc)
        .and_then(|tables| tables.get_mut(index))
        .context("account table vanished")?;
    match new {
        None => {
            table.remove(key);
        }
        Some(mut new) => {
            // Keep the comment and spacing that sat around the old value.
            if let Some(old) = table.get(key).and_then(Item::as_value) {
                *new.decor_mut() = old.decor().clone();
            }
            table[key] = Item::Value(new);
        }
    }
    Ok(doc.to_string())
}

/// Points an account at its Keychain item, if it has no `password_command` yet.
pub fn ensure_keychain_command(text: &str, name: &str) -> Result<String> {
    let mut doc = parse_doc(text)?;
    let index = find_account(&mut doc, name)?;
    let has_command = account_tables(&mut doc)
        .and_then(|tables| tables.get(index))
        .and_then(|t| t.get("password_command")?.as_str())
        .is_some_and(|c| !c.trim().is_empty());
    if has_command {
        return Ok(text.to_owned());
    }
    let command = keychain_password_command(&keychain_service(), name);
    set_account_value(text, name, "password_command", Some(command.into()))
}

/// Removes one `[[accounts]]` table.
pub fn remove_account(text: &str, name: &str) -> Result<String> {
    let mut doc = parse_doc(text)?;
    let index = find_account(&mut doc, name)?;
    account_tables(&mut doc)
        .context("no accounts")?
        .remove(index);
    Ok(doc.to_string())
}

/// Parses and validates `new_text` like a normal start would; the error is what `validate` says.
pub fn validate_text(new_text: &str) -> Result<Config> {
    Ok(Config::parse(new_text)?)
}

/// Writes `new_text` over the config: copy to `.bak`, temp file in the same directory,
/// fsync, rename. The caller has validated the text.
///
/// shortcut: no lock against two writers at once (the last rename wins); add a lock file if
/// two processes ever edit the config, for example the app and a terminal at the same time.
pub fn write_config(path: &Path, new_text: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    crate::store::create_private_dir(dir)?;
    let mut mode = 0o600;
    if let Ok(meta) = std::fs::metadata(path) {
        let old = std::fs::read(path).context("cannot read the current config")?;
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        write_private(Path::new(&bak), &old, false)?;
        // Keep a stricter mode (for example 0400); never loosen to the 0644 a file may have.
        mode = (meta.permissions().mode() & 0o600).max(0o400);
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    let written = write_private(tmp, new_text.as_bytes(), true)
        .and_then(|()| {
            Ok(std::fs::set_permissions(
                tmp,
                PermissionsExt::from_mode(mode),
            )?)
        })
        .and_then(|()| Ok(std::fs::rename(tmp, path)?));
    if written.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    written.with_context(|| format!("cannot write the config file {}", path.display()))
}

/// Creates or truncates `path` with mode 0600 and writes `bytes`, optionally fsyncing.
fn write_private(path: &Path, bytes: &[u8], fsync: bool) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let _ = std::fs::remove_file(path).ok().filter(|_| fsync);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // An older file keeps the mode it had; force 0600.
    file.set_permissions(PermissionsExt::from_mode(0o600))?;
    file.write_all(bytes)?;
    if fsync {
        file.sync_all()?;
    }
    Ok(())
}

/// Checks a password can be stored and read back through `security ... -w`, which prints
/// anything that is not printable ASCII as hex.
pub fn check_password(password: &str) -> Result<()> {
    let printable = password.chars().all(|c| c.is_ascii_graphic() || c == ' ');
    if password.is_empty() || !printable || password != password.trim() {
        bail!(
            "the password must be printable ASCII, not empty, with no leading or trailing space \
             (the Keychain command line would read other passwords back wrongly)"
        );
    }
    Ok(())
}

/// Double-quotes `text` for the tokenizer of `security -i`: backslash and quote are escaped.
fn security_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The one line fed to `security -i`, so the password never appears in argv.
fn security_add_line(service: &str, account: &str, password: &str) -> String {
    format!(
        "add-generic-password -U -s {} -a {} -w {}\n",
        security_quote(service),
        security_quote(account),
        security_quote(password)
    )
}

/// Stores (or updates) the password in the login Keychain, then reads it back to be sure:
/// `security -i` exits 0 even when a command inside it fails. Errors never contain the password.
pub fn store_password(account: &str, password: &str) -> Result<()> {
    check_account_name(account)?;
    check_password(password)?;
    let service = keychain_service();
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot run `security` (the Keychain is macOS only)")?;
    child
        .stdin
        .take()
        .context("no stdin for `security`")?
        .write_all(security_add_line(&service, account, password).as_bytes())?;
    // The stdin handle was dropped above, which tells `security -i` to finish.
    child.wait()?;
    let read = Command::new("security")
        .args(["find-generic-password", "-s", &service, "-a", account, "-w"])
        .stderr(Stdio::null())
        .output()?;
    if String::from_utf8_lossy(&read.stdout).trim_end_matches('\n') != password {
        bail!("the Keychain did not store the password correctly; nothing was changed");
    }
    Ok(())
}

/// Deletes the account's Keychain item. Returns whether one was deleted.
pub fn forget_password(account: &str) -> Result<bool> {
    check_account_name(account)?;
    let status = Command::new("security")
        .args([
            "delete-generic-password",
            "-s",
            &keychain_service(),
            "-a",
            account,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("cannot run `security` (the Keychain is macOS only)")?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "\
# my config
[policy]
dry_run = true   # stay safe

# the first one
[[accounts]]
name = \"work\"      # main
host = \"imap.example.com\"
username = \"me@example.com\"
port = 993 # standard

[[accounts]]
name = \"home\"
host = \"mail.example.org\"
username = \"me\"

[digest]
time = \"07:30\"
";

    fn new(name: &str) -> NewAccount {
        NewAccount {
            name: name.into(),
            host: "imap.example.net".into(),
            username: "u@example.net".into(),
            ..Default::default()
        }
    }

    #[test]
    fn add_keeps_everything_else_and_appends_after_the_last_account() {
        let out = add_account(BASE, &new("side")).unwrap();
        // Byte-identical apart from the new table, which sits after the last account.
        let table = "[[accounts]]\nname = \"side\"\nhost = \"imap.example.net\"\nusername = \"u@example.net\"\n\n[digest]";
        assert_eq!(out, BASE.replace("[digest]", table));
        let cfg = validate_text(&out).unwrap();
        assert_eq!(cfg.accounts.len(), 3);
        assert_eq!(cfg.accounts[2].name, "side");
        assert!(!out.contains("password_command"));
    }

    #[test]
    fn add_to_an_empty_file_and_with_all_keys() {
        let mut account = new("a b");
        account.port = Some(1143);
        account.tls = Some("starttls".into());
        account.dry_run = Some(false);
        account.flagging = Some(false);
        account.keychain_password = true;
        let out = add_account("", &account).unwrap();
        assert!(out.starts_with("# Let's Triage Mail"));
        let cfg = validate_text(&out).unwrap();
        let a = &cfg.accounts[0];
        assert_eq!(
            (a.port(), a.dry_run, a.flagging),
            (1143, Some(false), false)
        );
        assert_eq!(password_source(a), "keychain");
    }

    #[test]
    fn duplicate_names_are_rejected_by_validation() {
        let out = add_account(BASE, &new("work")).unwrap();
        let err = validate_text(&out).unwrap_err().to_string();
        assert!(err.contains("work"), "{err}");
    }

    #[test]
    fn names_are_checked() {
        for ok in ["work", "me@home.org", "a b_c-d.e"] {
            check_account_name(ok).unwrap();
        }
        for bad in [
            "",
            "it's",
            "a\"b",
            "a\\b",
            "a;b",
            "a$b",
            "a`b",
            "zażółć",
            "a\nb",
        ] {
            assert!(check_account_name(bad).is_err(), "{bad:?}");
        }
        assert!(add_account(BASE, &new("it's")).is_err());
    }

    /// Both texts with the one changed line removed must be identical.
    fn without(text: &str, needle: &str) -> String {
        text.lines()
            .filter(|l| !l.contains(needle))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn set_changes_one_value_and_keeps_its_comment() {
        let out = set_account_key(BASE, "work", "port", "1143").unwrap();
        assert!(out.contains("port = 1143 # standard"), "{out}");
        assert_eq!(without(&out, "port ="), without(BASE, "port ="));
        let out = set_account_key(BASE, "home", "tls", "starttls").unwrap();
        assert!(out.contains("tls = \"starttls\""));
        validate_text(&out).unwrap();
    }

    #[test]
    fn set_dry_run_default_removes_the_key() {
        let on = set_account_key(BASE, "home", "dry_run", "false").unwrap();
        assert!(on.contains("dry_run = false"));
        let off = set_account_key(&on, "home", "dry_run", "default").unwrap();
        assert_eq!(off, BASE);
    }

    #[test]
    fn set_rejects_unknown_keys_values_and_accounts() {
        let err = set_account_key(BASE, "work", "password_command", "x").unwrap_err();
        assert!(
            err.to_string().contains("allowed keys: host, port"),
            "{err}"
        );
        assert!(set_account_key(BASE, "work", "port", "70000").is_err());
        assert!(set_account_key(BASE, "work", "flagging", "yes").is_err());
        assert!(set_account_key(BASE, "work", "tls", "ssl").is_err());
        assert!(set_account_key(BASE, "work", "dry_run", "maybe").is_err());
        let err = set_account_key(BASE, "nope", "port", "1").unwrap_err();
        assert!(err.to_string().contains("accounts: work, home"), "{err}");
    }

    #[test]
    fn remove_drops_only_that_table() {
        let out = remove_account(BASE, "home").unwrap();
        assert!(!out.contains("home"));
        assert!(
            out.contains("# my config") && out.contains("[digest]") && out.contains("# standard")
        );
        assert_eq!(validate_text(&out).unwrap().accounts.len(), 1);
        assert!(remove_account(BASE, "nope").is_err());
    }

    #[test]
    fn keychain_command_is_added_only_when_missing() {
        let out = ensure_keychain_command(BASE, "home").unwrap();
        assert!(out.contains("password_command = \"security find-generic-password -s "));
        let cfg = validate_text(&out).unwrap();
        assert_eq!(password_source(cfg.account("home").unwrap()), "keychain");
        assert_eq!(ensure_keychain_command(&out, "home").unwrap(), out);
        let custom =
            set_account_value(BASE, "home", "password_command", Some("echo x".into())).unwrap();
        assert_eq!(ensure_keychain_command(&custom, "home").unwrap(), custom);
    }

    #[test]
    fn password_source_tells_the_three_apart() {
        let custom =
            set_account_value(BASE, "work", "password_command", Some("pass show x".into()))
                .unwrap();
        let both = ensure_keychain_command(&custom, "home").unwrap();
        let cfg = validate_text(&both).unwrap();
        assert_eq!(password_source(cfg.account("work").unwrap()), "command");
        assert_eq!(password_source(cfg.account("home").unwrap()), "keychain");
        assert_eq!(
            password_source(&validate_text(BASE).unwrap().accounts[0]),
            "env"
        );
    }

    #[test]
    fn security_line_quotes_awkward_passwords() {
        assert_eq!(
            security_add_line("svc", "my acct", "a\"b\\c d'e"),
            "add-generic-password -U -s \"svc\" -a \"my acct\" -w \"a\\\"b\\\\c d'e\"\n"
        );
    }

    #[test]
    fn passwords_that_the_keychain_would_misread_are_refused() {
        for bad in ["", " x", "x ", "zażółć", "a\tb", "a\nb"] {
            assert!(check_password(bad).is_err(), "{bad:?}");
        }
        check_password("a \"q\" \\ 'x' #y $z").unwrap();
    }

    #[test]
    fn write_backs_up_and_keeps_a_stricter_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        write_config(&path, BASE).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert!(!dir.path().join("sub/config.toml.bak").exists());
        std::fs::set_permissions(&path, PermissionsExt::from_mode(0o400)).unwrap();
        write_config(&path, "new = 1\n").unwrap();
        assert_eq!(mode(&path), 0o400);
        let bak = dir.path().join("sub/config.toml.bak");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), BASE);
        assert_eq!(mode(&bak), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new = 1\n");
        assert!(!dir.path().join("sub/config.toml.tmp").exists());
    }
}
