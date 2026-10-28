//! Configuration: the TOML file in, validated typed settings out.
//!
//! Loading is two steps. `serde` turns the TOML text into plain structs (and rejects unknown
//! keys, so a typo like `min_confidnce` is an error instead of being silently ignored). Then
//! [`Config::validate`] checks the rules `serde` cannot express: ranges, uniqueness, "TLS unless
//! loopback", "no red flag". Every error names the key (and account) that is wrong.
//!
//! Passwords never live in the config file. An account names a `password_command` (for example
//! a macOS Keychain lookup) or the password comes from an environment variable. Either way it
//! is returned as a [`Secret`], whose `Debug` output is `***`.
//!
//! See `docs/SPEC.md` section 3 and `config.example.toml`.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Deserialize;

use crate::decider::LlmSettings;
use crate::engine::{FlagColours, FlagPolicy, TriageFlag};
use crate::flags::FlagColour;
use crate::imap::{ImapSettings, TlsMode, is_loopback_host};
use crate::message::ProviderHeaders;

/// Everything that can go wrong while loading the config or looking up a password.
///
/// This is a library error type, so it is an enum callers can match on. `thiserror` writes
/// the `Display` and `Error` impls from the `#[error(...)]` attributes. None of the messages
/// ever contain a password.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read (missing, no permission, ...).
    #[error("cannot read config file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    /// The file is not valid TOML, or has an unknown key or a value of the wrong type.
    #[error("invalid config file {origin}:\n{source}")]
    Parse {
        origin: String,
        source: toml::de::Error,
    },

    /// The file parsed, but a value is not acceptable.
    #[error("config key `{key}`: {message}")]
    Invalid { key: String, message: String },

    /// `password_command` could not be run, failed, or printed nothing.
    #[error("account `{account}`: password_command failed: {message}")]
    PasswordCommand { account: String, message: String },

    /// No `password_command` and the environment variable is not set.
    #[error(
        "account `{account}`: no password found; set `password_command` in the config \
         or the environment variable {env_var}"
    )]
    PasswordMissing { account: String, env_var: String },
}

impl ConfigError {
    fn invalid(key: impl Into<String>, message: impl Into<String>) -> ConfigError {
        ConfigError::Invalid {
            key: key.into(),
            message: message.into(),
        }
    }
}

/// A password or API key.
///
/// A newtype around `String` whose `Debug` is written by hand to print `***`. Without that, a
/// stray `tracing::debug!("{config:?}")` or `{:?}` in an error would leak the secret into a
/// log file. There is deliberately no `Display` impl; reading the value takes an explicit
/// call to [`Secret::expose`].
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a value.
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    /// The secret itself. Call this only at the point of use (the IMAP login, an HTTP header).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// The whole configuration file.
///
/// `#[serde(default)]` on a struct means: any key missing from the TOML takes its value from
/// the type's `Default` impl. That is how every section is optional. `deny_unknown_fields`
/// turns typos into errors.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub llm: LlmConfig,
    pub policy: PolicyConfig,
    pub flags: FlagsConfig,
    pub digest: DigestConfig,
    pub storage: StorageConfig,
    pub accounts: Vec<AccountConfig>,
}

/// `[llm]`: the local model endpoint (any OpenAI-compatible server).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub timeout_seconds: u64,
    /// How much of the body (after HTML is flattened) the model sees.
    pub max_body_chars: usize,
    /// How many model requests may run at once, across all accounts.
    pub max_concurrent: usize,
    /// Upper limit on the length of the model's answer, in tokens. Reasoning models that
    /// write a long think block before the JSON may need more than the default.
    pub max_tokens: u32,
    /// Allow a `base_url` that is not on this machine or a private network. Off by default,
    /// because the privacy promise is "mail content never leaves your own network".
    pub allow_remote: bool,
    /// Only for local servers that insist on a key.
    pub api_key: Option<Secret>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            base_url: String::from("http://127.0.0.1:11434/v1"),
            model: String::from("qwen3:14b"),
            timeout_seconds: 120,
            max_body_chars: 4000,
            max_concurrent: 1,
            max_tokens: 1024,
            allow_remote: false,
            api_key: None,
        }
    }
}

impl From<&LlmConfig> for LlmSettings {
    fn from(llm: &LlmConfig) -> LlmSettings {
        LlmSettings {
            base_url: llm.base_url.clone(),
            model: llm.model.clone(),
            timeout: Duration::from_secs(llm.timeout_seconds),
            max_body_chars: llm.max_body_chars,
            max_concurrent: llm.max_concurrent,
            max_tokens: llm.max_tokens,
            api_key: llm.api_key.as_ref().map(|key| key.expose().to_string()),
        }
    }
}

/// `[policy]`: when the service may act.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Classify and log, but change nothing on the server. On by default: a new install
    /// must never touch a mailbox until the user has looked at the results.
    pub dry_run: bool,
    /// Below this confidence a message gets "Unsure" instead of an action flag.
    pub min_confidence: f32,
    /// Treat everyone the user has written to (from the Sent folder) as a known sender.
    pub known_senders_from_sent: bool,
    /// Only trust `Authentication-Results` headers stamped by this server. Empty means
    /// the topmost header is used.
    pub authserv_id: String,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        PolicyConfig {
            dry_run: true,
            min_confidence: FlagPolicy::default().min_confidence,
            known_senders_from_sent: true,
            authserv_id: String::new(),
        }
    }
}

/// The colour names a user may write in `[flags.*] colour`.
///
/// This mirrors [`FlagColour`] so that `serde` can do the parsing and report the list of valid
/// names on a typo. `red` is parsed too, so that we can reject it with an explanation instead
/// of a generic "unknown variant" error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ColourName {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
}

impl ColourName {
    fn to_colour(self) -> FlagColour {
        match self {
            ColourName::Red => FlagColour::Red,
            ColourName::Orange => FlagColour::Orange,
            ColourName::Yellow => FlagColour::Yellow,
            ColourName::Green => FlagColour::Green,
            ColourName::Blue => FlagColour::Blue,
            ColourName::Purple => FlagColour::Purple,
            ColourName::Gray => FlagColour::Gray,
        }
    }
}

/// One `[flags.<name>]` table. Every field is optional: what is missing keeps the default
/// for that flag (see `docs/FLAGS.md`), so `[flags.to_pay] enabled = true` is enough.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlagSetting {
    pub enabled: Option<bool>,
    colour: Option<ColourName>,
    /// The name shown in the digest. Rename the flag in Mail's sidebar to match.
    pub name: Option<String>,
}

/// `[flags.*]`: one table per flag the service can set.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlagsConfig {
    pub reply_needed: FlagSetting,
    pub deadline: FlagSetting,
    pub to_pay: FlagSetting,
    pub bookkeeping: FlagSetting,
    pub unsure: FlagSetting,
}

impl FlagsConfig {
    /// The `(config key, setting, flag)` triples, so loops need no copy-paste.
    fn entries(&self) -> [(&'static str, &FlagSetting, TriageFlag); 5] {
        [
            ("reply_needed", &self.reply_needed, TriageFlag::ReplyNeeded),
            ("deadline", &self.deadline, TriageFlag::Deadline),
            ("to_pay", &self.to_pay, TriageFlag::ToPay),
            ("bookkeeping", &self.bookkeeping, TriageFlag::Bookkeeping),
            ("unsure", &self.unsure, TriageFlag::Unsure),
        ]
    }
}

/// `[digest]`: the daily summary email.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DigestConfig {
    /// Local time of day, `HH:MM` (24 hours).
    pub time: String,
    /// The mailbox the digest is appended to.
    pub folder: String,
    /// The `From` header of the digest. `None` lets the digest code pick a default.
    pub from: Option<String>,
    /// Language of the digest text, as a short code. Only `en` is supported in v1; any other
    /// value is a config error.
    pub language: String,
}

impl Default for DigestConfig {
    fn default() -> Self {
        DigestConfig {
            time: String::from("07:00"),
            folder: String::from("INBOX"),
            from: None,
            language: String::from("en"),
        }
    }
}

impl DigestConfig {
    /// Whether `language` is English: `en`, or a regional form such as `en-GB` or `en_US`,
    /// in any case.
    pub fn language_is_supported(&self) -> bool {
        let language = self.language.trim().to_ascii_lowercase();
        language == "en" || language.starts_with("en-") || language.starts_with("en_")
    }

    /// The configured time as `(hour, minute)`. Always `Some` after [`Config::validate`].
    pub fn time_of_day(&self) -> Option<(u8, u8)> {
        let (hours, minutes) = self.time.split_once(':')?;
        // Exactly two digits each, so "7:5" and "+7:+5" are rejected.
        let two_digits = |s: &str| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
        if !two_digits(hours) || !two_digits(minutes) {
            return None;
        }
        let (hours, minutes) = (hours.parse::<u8>().ok()?, minutes.parse::<u8>().ok()?);
        (hours < 24 && minutes < 60).then_some((hours, minutes))
    }
}

/// `[storage]`: where the SQLite database lives.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// Defaults to `~/.local/share/lets-triage-mail/state.db`.
    pub db_path: Option<PathBuf>,
}

/// How an account's connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tls {
    /// Encrypted from the first byte (IMAPS, usually port 993).
    Tls,
    /// Starts in plain text, then upgrades (usually port 143).
    Starttls,
    /// No encryption. Accepted only for a loopback host (tests, a local bridge).
    None,
}

/// What the service may do to one account's mailbox. Worked out by [`AccountConfig::flag_mode`]
/// from `flagging` and `dry_run`; the one place that combines them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagMode {
    /// Flags are set and cleared on the server.
    Live,
    /// Mail is classified and the would-be flags are logged; nothing is written.
    DryRun,
    /// `flagging = false`: mail is classified for the digest only, and no flag is chosen.
    ClassifyOnly,
}

impl FlagMode {
    /// A short name for logs and `check` output.
    pub fn as_str(self) -> &'static str {
        match self {
            FlagMode::Live => "live",
            FlagMode::DryRun => "dry-run",
            FlagMode::ClassifyOnly => "classify-only",
        }
    }
}

/// One `[[accounts]]` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    pub host: String,
    pub username: String,
    /// Defaults to 993 for `tls` and 143 for `starttls`/`none`; see [`AccountConfig::port`].
    port: Option<u16>,
    #[serde(default = "default_tls")]
    pub tls: Tls,
    /// A shell command that prints the password. Run through `sh -c`.
    #[serde(default)]
    pub password_command: String,
    #[serde(default = "default_inbox")]
    pub inbox: String,
    /// Leave empty to auto-detect the Sent folder from its special-use attribute.
    #[serde(default)]
    pub sent_folder: String,
    /// The Spam folder to watch. Leave empty to detect it from its special-use attribute
    /// (`\Junk`) or a common name such as `Spam`.
    #[serde(default)]
    pub junk_folder: String,
    /// Look into the Spam folder, read-only, for mail from people the user knows. Nothing there
    /// is moved or changed; such messages are only listed in the digest.
    #[serde(default = "default_watch_junk")]
    pub watch_junk: bool,
    /// Whether the service may set flags on this account. `false` means "classify for the
    /// digest only", for servers that cannot store flag colours (no custom keywords, as on
    /// wp.pl). A bare `\Flagged` without colour bits shows as red in Apple Mail, and red is
    /// the user's, so such an account must not be flagged at all.
    #[serde(default = "default_flagging")]
    pub flagging: bool,
    /// Read this account's provider's own DKIM and spam headers (`"wp"` for wp.pl). Off by
    /// default.
    ///
    /// **Opt-in because a sender can write these headers themselves.** A spammer can add
    /// `X-WP-DKIM-Status: good` to a message, and it is only believable when the provider
    /// strips or overwrites the header on arrival. That is why this is set per account and
    /// not globally: the same fake header that wp.pl overwrites arrives untouched at Gmail.
    /// A standard `Authentication-Results` header, when present, still wins.
    ///
    /// The type is an enum, so `serde` rejects an unknown value with an error that lists the
    /// known ones (`unknown variant ..., expected wp`).
    #[serde(default)]
    pub provider_headers: Option<ProviderHeaders>,
    /// Overrides `[policy] dry_run` for this account. `None` means "follow the policy".
    ///
    /// This lets one low-volume account go live while the others stay in dry-run. The
    /// override works in both directions, but the safe use is `dry_run = false` on a single
    /// account whose results you have reviewed with `stats`.
    #[serde(default)]
    pub dry_run: Option<bool>,
}

fn default_flagging() -> bool {
    true
}

fn default_watch_junk() -> bool {
    true
}

fn default_tls() -> Tls {
    Tls::Tls
}

fn default_inbox() -> String {
    String::from("INBOX")
}

/// Whether a host name or address is on this machine or a private network, so that mail
/// content sent there stays inside the user's own network.
///
/// Accepts loopback, RFC 1918 private IPv4, link-local, IPv6 unique-local (`fc00::/7`),
/// `localhost`, and the names that are reserved for local networks: `.local` (mDNS), `.lan`
/// and `.home.arpa` (RFC 8375). It never looks anything up in DNS: a name that merely
/// *looks* local could still resolve to a public address, and the answer could change, so
/// the check is on the text only. A user who really means a remote server says so with
/// `llm.allow_remote`.
fn is_local_or_private_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    // `fc00::/7` and `fe80::/10` have no stable std helper yet, so we test
                    // the leading bits by hand.
                    || v6.segments()[0] & 0xfe00 == 0xfc00
                    || v6.segments()[0] & 0xffc0 == 0xfe80
                    || v6.to_ipv4_mapped().is_some_and(|v4| {
                        v4.is_loopback() || v4.is_private() || v4.is_link_local()
                    })
            }
        };
    }
    let name = bare.trim_end_matches('.').to_ascii_lowercase();
    // A single-label name such as `studio` can only come from the local network: public DNS
    // names always have a dot.
    (!name.is_empty() && !name.contains('.'))
        || name == "localhost"
        || name.ends_with(".localhost")
        || name.ends_with(".local")
        || name.ends_with(".lan")
        || name.ends_with(".home.arpa")
}

/// The environment variable that holds an account's password:
/// `LETS_TRIAGE_MAIL_PASSWORD_<ACCOUNT>`, with the name uppercased and every character that is
/// not a letter or digit replaced by `_`.
pub fn password_env_var(account_name: &str) -> String {
    let suffix: String = account_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("LETS_TRIAGE_MAIL_PASSWORD_{suffix}")
}

impl AccountConfig {
    /// Whether this account is in dry-run: the account's own `dry_run` if set, otherwise the
    /// global `[policy] dry_run`. Every place that asks "may we change this mailbox?" goes
    /// through here, so the per-account override cannot be forgotten somewhere.
    pub fn dry_run(&self, policy: &PolicyConfig) -> bool {
        self.dry_run.unwrap_or(policy.dry_run)
    }

    /// What the service does on this account. `flagging = false` wins over everything: such
    /// an account is never written to, whatever `dry_run` says.
    pub fn flag_mode(&self, policy: &PolicyConfig) -> FlagMode {
        if !self.flagging {
            FlagMode::ClassifyOnly
        } else if self.dry_run(policy) {
            FlagMode::DryRun
        } else {
            FlagMode::Live
        }
    }

    /// Whether the service really writes flags on this account.
    pub fn writes_flags(&self, policy: &PolicyConfig) -> bool {
        self.flag_mode(policy) == FlagMode::Live
    }

    /// The IMAP port: the configured one, or the standard port for the TLS mode.
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(match self.tls {
            Tls::Tls => 993,
            Tls::Starttls | Tls::None => 143,
        })
    }

    /// The connection settings for the IMAP session, with the already-resolved password.
    ///
    /// The password is passed in rather than looked up here, so this conversion stays pure
    /// (no shell commands) and a caller decides when the lookup happens. The "plain text
    /// only on loopback" rule is enforced in one place, [`ImapSettings::validate`], which
    /// the session runs before connecting; config validation uses the same
    /// `is_loopback_host` check.
    pub fn imap_settings(&self, password: &Secret) -> ImapSettings {
        ImapSettings {
            host: self.host.clone(),
            port: self.port(),
            tls: match self.tls {
                Tls::Tls => TlsMode::Implicit,
                Tls::Starttls => TlsMode::StartTls,
                Tls::None => TlsMode::PlainLoopbackOnly,
            },
            username: self.username.clone(),
            password: password.expose().to_string(),
        }
    }

    /// Looks up the password: `password_command` if set, otherwise the environment variable.
    pub fn password(&self) -> Result<Secret, ConfigError> {
        self.password_with_env(|name| std::env::var(name).ok())
    }

    /// Same as [`password`](Self::password), with the environment lookup passed in.
    ///
    /// Tests use this to supply fake variables. Changing the real environment from a test
    /// is unsafe in Rust 2024 (other threads may read it) and tests run in parallel.
    fn password_with_env(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Secret, ConfigError> {
        if !self.password_command.trim().is_empty() {
            return self.run_password_command();
        }
        let env_var = password_env_var(&self.name);
        match env(&env_var) {
            Some(value) if !value.is_empty() => Ok(Secret::new(value)),
            _ => Err(ConfigError::PasswordMissing {
                account: self.name.clone(),
                env_var,
            }),
        }
    }

    fn run_password_command(&self) -> Result<Secret, ConfigError> {
        let fail = |message: String| ConfigError::PasswordCommand {
            account: self.name.clone(),
            message,
        };

        // `sh -c` lets the command use quotes and pipes, like a line typed in a terminal.
        // The command's own stderr goes straight to ours so the user sees messages such as
        // "keychain item not found"; stdout is captured and never put in an error.
        let output = Command::new("sh")
            .arg("-c")
            .arg(&self.password_command)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| fail(format!("could not start the shell: {e}")))?;

        if !output.status.success() {
            return Err(fail(format!(
                "the command exited with {}; its output is not shown",
                output.status
            )));
        }

        let stdout = String::from_utf8(output.stdout).map_err(|_| {
            fail(String::from(
                "the command printed something that is not UTF-8",
            ))
        })?;
        let password = stdout.trim();
        if password.is_empty() {
            return Err(fail(String::from("the command printed nothing")));
        }
        Ok(Secret::new(password))
    }
}

impl Config {
    /// Reads, parses and validates the config file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Config::parse_named(&text, &path.display().to_string())
    }

    /// Parses and validates config text.
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse_named(text, "<text>")
    }

    fn parse_named(text: &str, origin: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|source| ConfigError::Parse {
            origin: origin.to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Checks every rule `serde` cannot. Called by [`load`](Self::load) and
    /// [`parse`](Self::parse); only needed directly for a `Config` built in code.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_llm()?;
        self.validate_policy()?;
        self.validate_flags()?;
        self.validate_digest()?;
        self.validate_accounts()
    }

    fn validate_llm(&self) -> Result<(), ConfigError> {
        let llm = &self.llm;
        if !(llm.base_url.starts_with("http://") || llm.base_url.starts_with("https://")) {
            return Err(ConfigError::invalid(
                "llm.base_url",
                "must start with http:// or https:// (for example http://127.0.0.1:11434/v1)",
            ));
        }
        self.validate_llm_host()?;
        if llm.model.trim().is_empty() {
            return Err(ConfigError::invalid("llm.model", "must not be empty"));
        }
        if llm.timeout_seconds == 0 {
            return Err(ConfigError::invalid(
                "llm.timeout_seconds",
                "must be at least 1",
            ));
        }
        if llm.max_body_chars == 0 {
            return Err(ConfigError::invalid(
                "llm.max_body_chars",
                "must be at least 1",
            ));
        }
        if llm.max_tokens == 0 {
            return Err(ConfigError::invalid("llm.max_tokens", "must be at least 1"));
        }
        if llm.max_concurrent == 0 {
            return Err(ConfigError::invalid(
                "llm.max_concurrent",
                "must be at least 1 (use 1 to share the machine with other models)",
            ));
        }
        Ok(())
    }

    /// Enforces the privacy rule: the model server must be local or on a private network.
    fn validate_llm_host(&self) -> Result<(), ConfigError> {
        let llm = &self.llm;
        if llm.allow_remote {
            return Ok(());
        }
        let url = reqwest::Url::parse(&llm.base_url).map_err(|e| {
            ConfigError::invalid("llm.base_url", format!("is not a valid URL: {e}"))
        })?;
        // `host_str` is `None` only for URLs without a host, which `http(s)://` never is,
        // but treating it as "not local" is the safe answer.
        let host = url.host_str().unwrap_or_default();
        if !is_local_or_private_host(host) {
            return Err(ConfigError::invalid(
                "llm.base_url",
                format!(
                    "`{host}` is not a local or private-network address. Let's Triage Mail \
                     sends your mail text to this server, and the privacy rule is that it \
                     never leaves your own network. Use a loopback address (127.0.0.1), a \
                     private address (10.x, 172.16-31.x, 192.168.x), or a .local, .lan or \
                     .home.arpa name. If you really want a remote server, set \
                     `allow_remote = true` in [llm]; your mail text is then sent there."
                ),
            ));
        }
        Ok(())
    }

    fn validate_policy(&self) -> Result<(), ConfigError> {
        // `contains` is false for NaN, so NaN is rejected too.
        if !(0.0..=1.0).contains(&self.policy.min_confidence) {
            return Err(ConfigError::invalid(
                "policy.min_confidence",
                format!(
                    "must be between 0 and 1, got {}",
                    self.policy.min_confidence
                ),
            ));
        }
        Ok(())
    }

    fn validate_flags(&self) -> Result<(), ConfigError> {
        for (key, setting, _) in self.flags.entries() {
            if setting.colour == Some(ColourName::Red) {
                return Err(ConfigError::invalid(
                    format!("flags.{key}.colour"),
                    "red is reserved for your own flags and is never set by the service; \
                     choose orange, yellow, green, blue, purple or gray",
                ));
            }
            if setting.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
                return Err(ConfigError::invalid(
                    format!("flags.{key}.name"),
                    "must not be empty",
                ));
            }
        }

        // Two flags with one colour could not be told apart in Mail, and clearing one
        // would clear the other. Only flags that are switched on count.
        let policy = self.flag_policy();
        let in_use = [
            (
                "reply_needed",
                policy.reply_needed,
                policy.colours.reply_needed,
            ),
            ("deadline", policy.deadline, policy.colours.deadline),
            ("to_pay", policy.to_pay, policy.colours.to_pay),
            (
                "bookkeeping",
                policy.bookkeeping,
                policy.colours.bookkeeping,
            ),
            ("unsure", policy.unsure, policy.colours.unsure),
        ];
        let enabled: Vec<_> = in_use.iter().filter(|(_, on, _)| *on).collect();
        for (index, (key, _, colour)) in enabled.iter().enumerate() {
            if let Some((other, _, _)) = enabled[..index].iter().find(|(_, _, c)| c == colour) {
                return Err(ConfigError::invalid(
                    format!("flags.{key}.colour"),
                    format!(
                        "flags `{other}` and `{key}` are both enabled with the same colour \
                         ({colour:?}); give each enabled flag its own colour"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn validate_digest(&self) -> Result<(), ConfigError> {
        let digest = &self.digest;
        if digest.time_of_day().is_none() {
            return Err(ConfigError::invalid(
                "digest.time",
                format!(
                    "must be a 24-hour time like \"07:30\", got \"{}\"",
                    digest.time
                ),
            ));
        }
        if digest.folder.trim().is_empty() {
            return Err(ConfigError::invalid("digest.folder", "must not be empty"));
        }
        if digest.from.as_deref().is_some_and(|f| f.trim().is_empty()) {
            return Err(ConfigError::invalid("digest.from", "must not be empty"));
        }
        if !digest.language_is_supported() {
            // Rejected instead of ignored with a warning: a config that says "pl" and then
            // quietly gets English is a surprise, and an error at startup is not.
            return Err(ConfigError::invalid(
                "digest.language",
                format!(
                    "only English (\"en\") is supported in v1, got \"{}\"",
                    digest.language
                ),
            ));
        }
        Ok(())
    }

    fn validate_accounts(&self) -> Result<(), ConfigError> {
        // Names are compared by their environment-variable form, because two accounts that
        // map to the same variable ("my-mail" and "my_mail") would share one password.
        let mut seen: Vec<(String, &str)> = Vec::new();

        for (index, account) in self.accounts.iter().enumerate() {
            let name = account.name.as_str();
            if name.trim().is_empty() {
                return Err(ConfigError::invalid(
                    format!("accounts[{index}].name"),
                    "must not be empty",
                ));
            }
            let key = |field: &str| format!("accounts[{name}].{field}");

            let env_var = password_env_var(name);
            if let Some((_, other)) = seen.iter().find(|(var, _)| *var == env_var) {
                return Err(ConfigError::invalid(
                    key("name"),
                    format!(
                        "account names must be unique; `{name}` clashes with `{other}` \
                         (both map to {env_var})"
                    ),
                ));
            }
            seen.push((env_var, name));

            if account.host.trim().is_empty() {
                return Err(ConfigError::invalid(key("host"), "must not be empty"));
            }
            if account.username.trim().is_empty() {
                return Err(ConfigError::invalid(key("username"), "must not be empty"));
            }
            if account.inbox.trim().is_empty() {
                return Err(ConfigError::invalid(key("inbox"), "must not be empty"));
            }
            if account.port == Some(0) {
                return Err(ConfigError::invalid(key("port"), "must not be 0"));
            }
            if account.tls == Tls::None && !is_loopback_host(&account.host) {
                return Err(ConfigError::invalid(
                    key("tls"),
                    format!(
                        "\"none\" is only allowed for a loopback host, and `{}` is not one; \
                         use \"tls\" or \"starttls\"",
                        account.host
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The account with this name.
    pub fn account(&self, name: &str) -> Option<&AccountConfig> {
        self.accounts.iter().find(|a| a.name == name)
    }

    /// The settings the decision engine needs: which flags are on, their colours and the
    /// confidence threshold. Missing settings keep the defaults from `docs/FLAGS.md`.
    pub fn flag_policy(&self) -> FlagPolicy {
        let defaults = FlagPolicy::default();
        let enabled = |setting: &FlagSetting, default: bool| setting.enabled.unwrap_or(default);
        let colour = |setting: &FlagSetting, flag: TriageFlag| {
            setting.colour.map_or(flag.colour(), ColourName::to_colour)
        };
        let flags = &self.flags;

        FlagPolicy {
            reply_needed: enabled(&flags.reply_needed, defaults.reply_needed),
            deadline: enabled(&flags.deadline, defaults.deadline),
            to_pay: enabled(&flags.to_pay, defaults.to_pay),
            bookkeeping: enabled(&flags.bookkeeping, defaults.bookkeeping),
            unsure: enabled(&flags.unsure, defaults.unsure),
            colours: FlagColours {
                reply_needed: colour(&flags.reply_needed, TriageFlag::ReplyNeeded),
                deadline: colour(&flags.deadline, TriageFlag::Deadline),
                to_pay: colour(&flags.to_pay, TriageFlag::ToPay),
                bookkeeping: colour(&flags.bookkeeping, TriageFlag::Bookkeeping),
                unsure: colour(&flags.unsure, TriageFlag::Unsure),
            },
            min_confidence: self.policy.min_confidence,
        }
    }

    /// The display name of a flag (the configured one, or the default such as "Reply needed").
    pub fn flag_name(&self, flag: TriageFlag) -> &str {
        let configured = self
            .flags
            .entries()
            .into_iter()
            .find(|&(_, _, f)| f == flag)
            .and_then(|(_, setting, _)| setting.name.as_deref());
        configured.unwrap_or(match flag {
            TriageFlag::ReplyNeeded => "Reply needed",
            TriageFlag::Deadline => "Deadline",
            TriageFlag::ToPay => "To pay",
            TriageFlag::Bookkeeping => "Bookkeeping",
            TriageFlag::Unsure => "Unsure",
        })
    }

    /// Where the SQLite database lives: the configured path, or the default under
    /// `~/.local/share`. A leading `~` is expanded.
    pub fn db_path(&self) -> PathBuf {
        let path = self
            .storage
            .db_path
            .clone()
            .unwrap_or_else(|| PathBuf::from("~/.local/share/lets-triage-mail/state.db"));
        expand_home(&path)
    }
}

/// Replaces a leading `~` with the home directory (`$HOME`). The shell does this for
/// command-line arguments, but not for paths read from a file.
fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid account, as TOML.
    const ACCOUNT: &str = r#"
        [[accounts]]
        name = "work"
        host = "imap.example.com"
        username = "me@example.com"
    "#;

    /// Parses `text` and returns the error message, failing the test if it is accepted.
    fn error_of(text: &str) -> String {
        Config::parse(text)
            .expect_err("config should be rejected")
            .to_string()
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let config = Config::parse(text).expect("config.example.toml must stay valid");
        assert!(config.policy.dry_run);
        assert_eq!(config.accounts.len(), 1);
    }

    #[test]
    fn empty_config_uses_the_safe_defaults() {
        let config = Config::parse("").unwrap();
        assert!(config.policy.dry_run, "dry_run must default to true");
        assert_eq!(config.llm.max_concurrent, 1);
        assert_eq!(config.policy.min_confidence, 0.75);
        assert_eq!(config.digest.time_of_day(), Some((7, 0)));
        assert!(config.accounts.is_empty());
        assert_eq!(config.flag_policy(), FlagPolicy::default());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let message = error_of("[policy]\nmin_confidnce = 0.5\n");
        assert!(message.contains("min_confidnce"), "{message}");
    }

    const ACCOUNT_A: &str = "[[accounts]]\nname = \"a\"\nhost = \"h\"\nusername = \"u\"\n";

    #[test]
    fn account_dry_run_overrides_the_policy_in_both_directions() {
        let config = Config::parse(&format!(
            "[policy]\ndry_run = true\n{ACCOUNT_A}\
             [[accounts]]\nname = \"b\"\nhost = \"h\"\nusername = \"u\"\ndry_run = false\n"
        ))
        .unwrap();
        let (a, b) = (&config.accounts[0], &config.accounts[1]);
        assert!(a.dry_run(&config.policy), "no override: follows the policy");
        assert!(!b.dry_run(&config.policy), "account override: live");
        assert!(!a.writes_flags(&config.policy));
        assert!(b.writes_flags(&config.policy));
        assert_eq!(a.flag_mode(&config.policy), FlagMode::DryRun);
        assert_eq!(b.flag_mode(&config.policy), FlagMode::Live);

        let live = Config::parse(&format!(
            "[policy]\ndry_run = false\n{ACCOUNT_A}dry_run = true\n"
        ))
        .unwrap();
        assert!(live.accounts[0].dry_run(&live.policy), "can stay safe");
    }

    #[test]
    fn flagging_off_never_writes_whatever_dry_run_says() {
        let config = Config::parse(&format!(
            "[policy]\ndry_run = false\n{ACCOUNT_A}flagging = false\n"
        ))
        .unwrap();
        let account = &config.accounts[0];
        assert!(!account.dry_run(&config.policy));
        assert!(!account.writes_flags(&config.policy));
        assert_eq!(account.flag_mode(&config.policy), FlagMode::ClassifyOnly);
    }

    #[test]
    fn provider_headers_are_off_by_default_and_set_per_account() {
        let config = Config::parse(ACCOUNT).unwrap();
        assert_eq!(config.accounts[0].provider_headers, None);
        let config = Config::parse(&format!("{ACCOUNT}provider_headers = \"wp\"\n")).unwrap();
        assert_eq!(
            config.accounts[0].provider_headers,
            Some(ProviderHeaders::Wp)
        );
    }

    #[test]
    fn the_spam_watch_is_on_by_default_and_can_name_its_folder() {
        let config = Config::parse(ACCOUNT).unwrap();
        assert!(config.accounts[0].watch_junk);
        assert_eq!(config.accounts[0].junk_folder, "");

        let config = Config::parse(&format!(
            "{ACCOUNT}junk_folder = \"Spam\"\nwatch_junk = false\n"
        ))
        .unwrap();
        assert!(!config.accounts[0].watch_junk);
        assert_eq!(config.accounts[0].junk_folder, "Spam");
    }

    #[test]
    fn an_unknown_provider_is_rejected_with_the_known_ones_listed() {
        let message = error_of(&format!("{ACCOUNT}provider_headers = \"gmail\"\n"));
        assert!(message.contains("gmail"), "{message}");
        assert!(message.contains("`wp`"), "{message}");
    }

    #[test]
    fn provider_headers_are_no_longer_a_policy_setting() {
        // It moved to `[[accounts]]`: one global switch would trust wp.pl headers on Gmail too.
        let message = error_of("[policy]\nprovider_headers = \"wp\"\n");
        assert!(message.contains("provider_headers"), "{message}");
    }

    #[test]
    fn min_confidence_must_be_between_0_and_1() {
        for bad in ["1.5", "-0.1"] {
            let message = error_of(&format!("[policy]\nmin_confidence = {bad}\n"));
            assert!(message.contains("policy.min_confidence"), "{message}");
        }
        assert!(Config::parse("[policy]\nmin_confidence = 1.0\n").is_ok());
        assert!(Config::parse("[policy]\nmin_confidence = nan\n").is_err());
    }

    #[test]
    fn max_concurrent_must_be_at_least_one() {
        let message = error_of("[llm]\nmax_concurrent = 0\n");
        assert!(message.contains("llm.max_concurrent"), "{message}");
    }

    #[test]
    fn llm_base_url_needs_a_scheme() {
        let message = error_of("[llm]\nbase_url = \"127.0.0.1:11434\"\n");
        assert!(message.contains("llm.base_url"), "{message}");
    }

    #[test]
    fn red_flag_colour_is_rejected() {
        let message = error_of("[flags.to_pay]\ncolour = \"red\"\n");
        assert!(message.contains("flags.to_pay.colour"), "{message}");
        assert!(message.contains("reserved"), "{message}");
    }

    #[test]
    fn unknown_flag_colour_is_rejected() {
        assert!(Config::parse("[flags.to_pay]\ncolour = \"pink\"\n").is_err());
    }

    #[test]
    fn flag_settings_override_defaults_and_keep_the_rest() {
        let config = Config::parse(
            r#"
            [policy]
            min_confidence = 0.9
            [flags.to_pay]
            enabled = true
            colour = "blue"
            name = "Bills"
            "#,
        )
        .unwrap();
        let policy = config.flag_policy();
        assert!(policy.to_pay);
        assert!(policy.reply_needed, "untouched flags keep their defaults");
        assert!(!policy.deadline);
        assert_eq!(policy.min_confidence, 0.9);
        assert_eq!(policy.colour_for(TriageFlag::ToPay), FlagColour::Blue);
        assert_eq!(
            policy.colour_for(TriageFlag::ReplyNeeded),
            FlagColour::Orange
        );
        assert_eq!(config.flag_name(TriageFlag::ToPay), "Bills");
        assert_eq!(config.flag_name(TriageFlag::Unsure), "Unsure");
    }

    #[test]
    fn only_english_is_accepted_for_the_digest() {
        for good in ["en", "EN", "en-GB", "en_US"] {
            assert!(
                Config::parse(&format!("[digest]\nlanguage = \"{good}\"\n")).is_ok(),
                "{good}"
            );
        }
        for bad in ["pl", "de", "", "english", "enx"] {
            let message = error_of(&format!("[digest]\nlanguage = \"{bad}\"\n"));
            assert!(message.contains("digest.language"), "{bad}: {message}");
            assert!(message.contains("only English"), "{bad}: {message}");
        }
    }

    #[test]
    fn digest_time_must_be_hh_mm() {
        for good in ["00:00", "07:30", "23:59"] {
            assert!(Config::parse(&format!("[digest]\ntime = \"{good}\"\n")).is_ok());
        }
        for bad in [
            "24:00", "7:30", "07:60", "0730", "noon", "07:30:00", "+7:30",
        ] {
            let message = error_of(&format!("[digest]\ntime = \"{bad}\"\n"));
            assert!(message.contains("digest.time"), "{bad}: {message}");
        }
    }

    #[test]
    fn account_defaults() {
        let config = Config::parse(ACCOUNT).unwrap();
        let account = config.account("work").unwrap();
        assert_eq!(account.tls, Tls::Tls);
        assert_eq!(account.port(), 993);
        assert_eq!(account.inbox, "INBOX");
    }

    #[test]
    fn starttls_defaults_to_port_143() {
        let text = ACCOUNT.to_string() + "tls = \"starttls\"\n";
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.accounts[0].port(), 143);
    }

    #[test]
    fn account_names_must_be_unique() {
        let message = error_of(&format!("{ACCOUNT}{ACCOUNT}"));
        assert!(message.contains("unique"), "{message}");
        assert!(message.contains("work"), "{message}");
    }

    #[test]
    fn account_names_that_share_an_env_var_clash() {
        let text = r#"
            [[accounts]]
            name = "my-mail"
            host = "a.example.com"
            username = "a"
            [[accounts]]
            name = "my_mail"
            host = "b.example.com"
            username = "b"
        "#;
        assert!(error_of(text).contains("LETS_TRIAGE_MAIL_PASSWORD_MY_MAIL"));
    }

    #[test]
    fn plain_text_is_only_allowed_for_loopback() {
        let message = error_of(&(ACCOUNT.to_string() + "tls = \"none\"\n"));
        assert!(message.contains("accounts[work].tls"), "{message}");

        for host in ["127.0.0.1", "localhost", "::1"] {
            let text = format!(
                "[[accounts]]\nname = \"t\"\nhost = \"{host}\"\nusername = \"u\"\ntls = \"none\"\n"
            );
            assert!(Config::parse(&text).is_ok(), "{host} should be accepted");
        }
    }

    #[test]
    fn missing_account_field_is_reported() {
        let message = error_of("[[accounts]]\nname = \"work\"\nhost = \"h\"\n");
        assert!(message.contains("username"), "{message}");
    }

    #[test]
    fn env_var_name_is_uppercased_with_underscores() {
        assert_eq!(
            password_env_var("personal"),
            "LETS_TRIAGE_MAIL_PASSWORD_PERSONAL"
        );
        assert_eq!(
            password_env_var("my-work.mail"),
            "LETS_TRIAGE_MAIL_PASSWORD_MY_WORK_MAIL"
        );
    }

    fn account_with_command(command: &str) -> AccountConfig {
        let config = Config::parse(ACCOUNT).unwrap();
        AccountConfig {
            password_command: command.to_string(),
            ..config.accounts[0].clone()
        }
    }

    #[test]
    fn password_comes_from_the_environment() {
        let account = account_with_command("");
        let secret = account
            .password_with_env(|name| {
                (name == "LETS_TRIAGE_MAIL_PASSWORD_WORK").then(|| String::from("hunter2"))
            })
            .unwrap();
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn missing_password_names_the_env_var() {
        let message = account_with_command("")
            .password_with_env(|_| None)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("LETS_TRIAGE_MAIL_PASSWORD_WORK"),
            "{message}"
        );
    }

    #[test]
    fn password_command_output_is_trimmed() {
        let secret = account_with_command("echo '  s3cret  '")
            .password()
            .unwrap();
        assert_eq!(secret.expose(), "s3cret");
    }

    #[test]
    fn password_command_wins_over_the_environment() {
        let secret = account_with_command("echo from-command")
            .password_with_env(|_| Some(String::from("from-env")))
            .unwrap();
        assert_eq!(secret.expose(), "from-command");
    }

    #[test]
    fn failing_password_command_does_not_echo_its_output() {
        let error = account_with_command("echo topsecret; exit 3")
            .password()
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("work"), "{message}");
        assert!(!message.contains("topsecret"), "{message}");
        assert!(!format!("{error:?}").contains("topsecret"));
    }

    #[test]
    fn empty_password_command_output_is_an_error() {
        assert!(account_with_command("true").password().is_err());
    }

    #[test]
    fn secret_never_prints() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "***");
        assert_eq!(format!("{secret:#?}"), "***");

        // The api key inside a whole config is covered too.
        let config = Config::parse("[llm]\napi_key = \"hunter2\"\n").unwrap();
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    #[test]
    fn load_reports_a_missing_file() {
        let error = Config::load(Path::new("/nonexistent/config.toml")).unwrap_err();
        assert!(error.to_string().contains("/nonexistent/config.toml"));
    }
    fn llm_url(url: &str) -> String {
        format!("[llm]\nbase_url = \"{url}\"\n")
    }

    #[test]
    fn local_and_private_model_hosts_are_accepted() {
        for url in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:1234/v1",
            "http://[::1]:8080/v1",
            "http://10.0.0.5/v1",
            "http://172.16.3.4/v1",
            "http://172.31.255.1/v1",
            "http://192.168.1.20:1234/v1",
            "http://169.254.10.10/v1",
            "http://[fd12:3456::1]/v1",
            "http://[fe80::1]/v1",
            "http://mac-studio.local:1234/v1",
            "http://studio.lan/v1",
            "http://studio:1234/v1",
            "http://studio.home.arpa/v1",
            "http://STUDIO.Local/v1",
        ] {
            assert!(
                Config::parse(&llm_url(url)).is_ok(),
                "{url} should be accepted"
            );
        }
    }

    #[test]
    fn remote_model_hosts_are_rejected_with_the_privacy_rule() {
        for url in [
            "https://api.openai.com/v1",
            "http://8.8.8.8/v1",
            "http://172.32.0.1/v1",
            "http://192.169.0.1/v1",
            "http://[2001:db8::1]/v1",
            "http://evil.local.example.com/v1",
        ] {
            let message = error_of(&llm_url(url));
            assert!(message.contains("llm.base_url"), "{url}: {message}");
            assert!(message.contains("allow_remote"), "{url}: {message}");
        }
    }

    #[test]
    fn allow_remote_opts_out_of_the_host_check() {
        let text = "[llm]\nbase_url = \"https://llm.example.com/v1\"\nallow_remote = true\n";
        assert!(Config::parse(text).is_ok());
    }

    #[test]
    fn max_tokens_defaults_to_1024_and_reaches_the_settings() {
        assert_eq!(Config::parse("").unwrap().llm.max_tokens, 1024);
        let config = Config::parse("[llm]\nmax_tokens = 4096\napi_key = \"k\"\n").unwrap();
        let settings = LlmSettings::from(&config.llm);
        assert_eq!(settings.max_tokens, 4096);
        assert_eq!(settings.api_key.as_deref(), Some("k"));
        assert_eq!(settings.timeout, Duration::from_secs(120));
        assert!(error_of("[llm]\nmax_tokens = 0\n").contains("llm.max_tokens"));
    }

    #[test]
    fn two_enabled_flags_may_not_share_a_colour() {
        let message = error_of("[flags.deadline]\nenabled = true\ncolour = \"orange\"\n");
        assert!(message.contains("flags.deadline.colour"), "{message}");
        assert!(message.contains("reply_needed"), "{message}");
    }

    #[test]
    fn a_disabled_flag_may_share_a_colour() {
        // `deadline` is off by default, so it does not clash with `reply_needed`.
        assert!(Config::parse("[flags.deadline]\ncolour = \"orange\"\n").is_ok());
    }

    #[test]
    fn account_converts_to_imap_settings() {
        let config = Config::parse(
            r#"
            [[accounts]]
            name = "a"
            host = "imap.example.com"
            username = "me"
            tls = "starttls"
            "#,
        )
        .unwrap();
        let settings = config.accounts[0].imap_settings(&Secret::new("pw"));
        assert_eq!(settings.host, "imap.example.com");
        assert_eq!(settings.port, 143);
        assert_eq!(settings.tls, TlsMode::StartTls);
        assert_eq!(settings.username, "me");
        assert_eq!(settings.password, "pw");
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn tls_modes_map_and_plain_stays_loopback_only() {
        let account = |tls: &str, host: &str| {
            let text = format!(
                "[[accounts]]\nname = \"a\"\nhost = \"{host}\"\nusername = \"u\"\ntls = \"{tls}\"\n"
            );
            Config::parse(&text).unwrap().accounts.remove(0)
        };
        let secret = Secret::new("pw");
        assert_eq!(
            account("tls", "imap.example.com")
                .imap_settings(&secret)
                .tls,
            TlsMode::Implicit
        );
        let plain = account("none", "127.0.0.1").imap_settings(&secret);
        assert_eq!(plain.tls, TlsMode::PlainLoopbackOnly);
        assert_eq!(plain.port, 143);
        // The session-side check agrees with the config-side one (same function).
        let mut remote = plain;
        remote.host = "imap.example.com".into();
        assert!(remote.validate().is_err());
    }
}
