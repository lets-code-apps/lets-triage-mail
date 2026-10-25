//! The checks behind the `check` command: is the config sane, does the model answer, can we
//! log in to each account and are the features we need switched on?
//!
//! # Shape
//!
//! Every probe is an `async fn` that talks to the outside world and returns a plain
//! `Vec<CheckItem>`. A [`CheckItem`] is one line of the report: an area, a [`Status`], a
//! message and, for problems, a hint on what to do about it. Probes never print.
//!
//! Anything that can be decided from plain values (what does `PERMANENTFLAGS` without `\*`
//! mean? is the model name in this `/models` answer?) lives in a small pure function that the
//! probe calls, so those decisions are unit-tested without a server. [`Report`] collects the
//! items and formats them as text or JSON, also without IO.
//!
//! # Hard rule
//!
//! `check` must never change anything on a mail server. The probes only log in, ask for
//! capabilities, `LIST` and `SELECT`. There is no `STORE`, `APPEND` or folder creation here.
//! The only thing `check` writes is the local state database, which it creates if it is missing
//! (the normal `run` would do the same).

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::{AccountConfig, LlmConfig, password_env_var};
use crate::decider::{
    AuthResult, DecideError, Decider, Evidence, LlmDecider, LlmSettings, install_tls_provider,
};
use crate::imap::{ImapError, ImapSession, ImapSettings, ResolvedFolders, ServerCapabilities};
use crate::store::Store;

/// How long listing the models may take. It is a cheap request, so this is short.
const LISTING_TIMEOUT: Duration = Duration::from_secs(10);

/// How long each IMAP command after the login may take. (Connecting has its own timeout inside
/// [`ImapSession::connect`].)
const IMAP_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How many model names to show when the configured one is missing.
const MAX_LISTED_MODELS: usize = 8;

// ---------------------------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------------------------

/// How a check went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Works as needed.
    Ok,
    /// Works, but something is worth knowing or fixing.
    Warn,
    /// Broken: the service cannot do its job like this.
    Fail,
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckItem {
    /// The section of the report: `Config`, `Model` or `Account <name>`.
    pub area: String,
    pub status: Status,
    pub message: String,
    /// What to do about a warning or failure. `None` for OK lines.
    pub hint: Option<String>,
}

impl CheckItem {
    pub fn ok(area: &str, message: impl Into<String>) -> CheckItem {
        CheckItem {
            area: area.to_string(),
            status: Status::Ok,
            message: message.into(),
            hint: None,
        }
    }

    pub fn warn(area: &str, message: impl Into<String>, hint: impl Into<String>) -> CheckItem {
        CheckItem {
            area: area.to_string(),
            status: Status::Warn,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }

    pub fn fail(area: &str, message: impl Into<String>, hint: impl Into<String>) -> CheckItem {
        CheckItem {
            area: area.to_string(),
            status: Status::Fail,
            message: message.into(),
            hint: Some(hint.into()),
        }
    }
}

/// All the items of one `check` run, with the pure formatting on top.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub items: Vec<CheckItem>,
}

impl Report {
    fn count(&self, status: Status) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    /// True if anything failed. The command turns this into a non-zero exit code.
    pub fn has_failures(&self) -> bool {
        self.count(Status::Fail) > 0
    }

    /// The report as readable text: one block per area, in the order the items came.
    pub fn to_text(&self) -> String {
        format_items(&self.items)
    }

    /// The closing line, such as `2 failed, 1 warning`.
    pub fn summary(&self) -> String {
        let failed = self.count(Status::Fail);
        let warnings = self.count(Status::Warn);
        match (failed, warnings) {
            (0, 0) => String::from("All checks passed."),
            (0, w) => format!("All checks passed, with {w} {}.", plural(w, "warning")),
            (f, 0) => format!("{f} {} failed.", plural(f, "check")),
            (f, w) => format!(
                "{f} {} failed, {w} {}.",
                plural(f, "check"),
                plural(w, "warning")
            ),
        }
    }

    /// The report as one JSON value, for `check --json`.
    pub fn to_json(&self) -> Value {
        json!({
            "ok": !self.has_failures(),
            "failed": self.count(Status::Fail),
            "warnings": self.count(Status::Warn),
            "items": self.items,
        })
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        word.to_string()
    } else {
        format!("{word}s")
    }
}

/// Formats items as text. A new heading starts whenever the area changes, so the items of one
/// area should be next to each other (they are, because a probe returns them together).
pub fn format_items(items: &[CheckItem]) -> String {
    let mut out = String::new();
    let mut current_area: Option<&str> = None;
    for item in items {
        if current_area != Some(item.area.as_str()) {
            if current_area.is_some() {
                out.push('\n');
            }
            out.push_str(&item.area);
            out.push('\n');
            current_area = Some(item.area.as_str());
        }
        let label = match item.status {
            Status::Ok => "[OK]  ",
            Status::Warn => "[WARN]",
            Status::Fail => "[FAIL]",
        };
        out.push_str(&format!("  {label} {}\n", item.message));
        if let Some(hint) = &item.hint {
            // Lined up under the message: two spaces, the 6-wide label, a space.
            out.push_str(&format!("         -> {hint}\n"));
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------------------------

const CONFIG: &str = "Config";

/// The item for `policy.dry_run`. Turning it off is allowed, but it is the switch that makes
/// the service write flags, so `check` points it out.
pub fn dry_run_item(dry_run: bool) -> CheckItem {
    if dry_run {
        CheckItem::ok(
            CONFIG,
            "dry-run is on: mail is classified and logged, nothing is changed on the server",
        )
    } else {
        CheckItem::warn(
            CONFIG,
            "dry-run is off: flags will be written to your mailboxes",
            "keep policy.dry_run = true until `stats` shows verdicts you agree with",
        )
    }
}

/// Opens (creating if needed) the state database to prove the path is usable.
///
/// [`Store::open`] creates the directory and file with private permissions (0700 and 0600),
/// so a successful check also shows the permissions are in place.
pub fn database_item(path: &Path) -> CheckItem {
    match Store::open(path) {
        Ok(_store) => CheckItem::ok(
            CONFIG,
            format!(
                "database {} opens (private permissions; created if missing)",
                path.display()
            ),
        ),
        Err(error) => CheckItem::fail(
            CONFIG,
            format!("cannot open the database {}: {error}", path.display()),
            "make sure the directory is writable, or set [storage] db_path to another place",
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------

const MODEL: &str = "Model";

/// Runs all model checks: endpoint, model name, one test classification.
pub async fn probe_model(llm: &LlmConfig) -> Vec<CheckItem> {
    // reqwest needs a TLS provider installed before any client is built (see the function).
    install_tls_provider();

    let url = format!("{}/models", llm.base_url.trim_end_matches('/'));
    let mut items = Vec::new();
    match fetch_models(llm, &url).await {
        Err(error) => {
            items.push(CheckItem::fail(
                MODEL,
                format!("cannot reach the model server at {}: {error}", llm.base_url),
                "start your model server (for example `ollama serve`) or fix [llm] base_url",
            ));
            // A test classification would only repeat the same failure.
            return items;
        }
        Ok((status, body)) => items.extend(interpret_models_response(
            &llm.base_url,
            &llm.model,
            status,
            &body,
        )),
    }
    items.extend(probe_classification(llm).await);
    items
}

/// `GET {base_url}/models`. Returns the HTTP status and body, or the transport error text.
async fn fetch_models(llm: &LlmConfig, url: &str) -> Result<(u16, String), String> {
    let client = reqwest::Client::builder()
        .timeout(LISTING_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client.get(url);
    if let Some(key) = &llm.api_key {
        request = request.bearer_auth(key.expose());
    }
    let response = request.send().await.map_err(describe_reqwest_error)?;
    let status = response.status().as_u16();
    // A body that cannot be read is treated like an empty one; the status is what matters.
    let body = response.text().await.unwrap_or_default();
    Ok((status, body))
}

/// reqwest's `Display` for a refused connection is just "error sending request", which says
/// little. The useful part is at the end of the `source` chain (each error wraps the one that
/// caused it), so we walk down to the root cause, for example "Connection refused".
fn describe_reqwest_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        return String::from("timed out");
    }
    let mut root: &dyn std::error::Error = &error;
    while let Some(cause) = root.source() {
        root = cause;
    }
    root.to_string()
}

/// The shape of an OpenAI `GET /models` answer; only the ids matter.
#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// Turns the answer of `GET /models` into check items. Pure: status and body in, items out.
///
/// Any HTTP answer proves the server is reachable. Whether the model list is usable is a
/// separate question, and a server that cannot list models (some do not) only earns a
/// warning, because chat requests may still work; the test classification decides that.
pub fn interpret_models_response(
    base_url: &str,
    model: &str,
    status: u16,
    body: &str,
) -> Vec<CheckItem> {
    let reachable = CheckItem::ok(MODEL, format!("model server answers at {base_url}"));
    let listing = match status {
        200..=299 => match serde_json::from_str::<ModelList>(body) {
            Ok(list) => {
                let ids: Vec<String> = list.data.into_iter().map(|m| m.id).collect();
                model_presence_item(model, &ids)
            }
            Err(_) => CheckItem::warn(
                MODEL,
                "the server's model list is not in the OpenAI format, so the model name was not checked",
                "harmless if the test classification below works",
            ),
        },
        401 | 403 => CheckItem::fail(
            MODEL,
            format!("the model server refused access (HTTP {status})"),
            "set [llm] api_key to the key your server expects",
        ),
        404 | 405 | 501 => CheckItem::warn(
            MODEL,
            format!(
                "the server does not support listing models (HTTP {status}), so the model name was not checked"
            ),
            "harmless if the test classification below works",
        ),
        _ => CheckItem::warn(
            MODEL,
            format!("listing models failed with HTTP {status}"),
            "check the model server's log; the test classification below shows whether it works",
        ),
    };
    vec![reachable, listing]
}

/// Is the configured model among the ids the server listed?
fn model_presence_item(model: &str, ids: &[String]) -> CheckItem {
    // Ollama lists `llama3:latest` for a model pulled as `llama3`; accept both spellings.
    let normalise = |name: &str| name.strip_suffix(":latest").unwrap_or(name).to_string();
    let wanted = normalise(model);
    if ids.iter().any(|id| normalise(id) == wanted) {
        return CheckItem::ok(MODEL, format!("model `{model}` is available"));
    }
    let hint = if ids.is_empty() {
        String::from("the server lists no models; load or pull one, then set [llm] model to it")
    } else {
        let shown: Vec<&str> = ids
            .iter()
            .take(MAX_LISTED_MODELS)
            .map(String::as_str)
            .collect();
        let more = ids.len().saturating_sub(MAX_LISTED_MODELS);
        let extra = if more > 0 {
            format!(" (and {more} more)")
        } else {
            String::new()
        };
        format!(
            "available: {}{extra}. Pull the model (for example `ollama pull {model}`) or set [llm] model to one of these",
            shown.join(", ")
        )
    };
    CheckItem::fail(
        MODEL,
        format!("model `{model}` is not available on the server"),
        hint,
    )
}

/// A made-up message for the test classification. It contains no real mail.
fn synthetic_evidence() -> Evidence {
    Evidence {
        from_address: String::from("newsletter@shop.example"),
        from_name: String::from("Example Shop"),
        reply_to: None,
        subject: String::from("Summer sale: 20% off everything this weekend"),
        date: None,
        body_text: String::from(
            "Hello! Our summer sale starts today. Use code SUMMER20 for 20% off. \
             Unsubscribe at any time.",
        ),
        headers_only: false,
        attachment_names: Vec::new(),
        spf: AuthResult::Pass,
        dkim: AuthResult::Pass,
        dmarc: AuthResult::Pass,
        provider_spam: None,
        bulk_headers: true,
        auto_submitted: false,
        known_domain: false,
        reply_to_mismatch: false,
        display_name_spoof: false,
        malformed_sender: false,
        impersonates_recipient: false,
        link_count: 1,
    }
}

/// Sends the synthetic message through the real [`LlmDecider`] and reports the time and the
/// structured-output mode that worked.
async fn probe_classification(llm: &LlmConfig) -> Vec<CheckItem> {
    let decider = match LlmDecider::new(LlmSettings::from(llm)) {
        Ok(decider) => decider,
        Err(error) => {
            return vec![CheckItem::fail(
                MODEL,
                format!("could not set up the model client: {error}"),
                "check [llm] base_url",
            )];
        }
    };

    // The decider has a per-request timeout, but a fallback between output modes sends
    // several requests. This outer limit bounds the whole probe.
    let limit = Duration::from_secs(llm.timeout_seconds);
    let started = Instant::now();
    let outcome = tokio::time::timeout(limit, decider.decide(&synthetic_evidence())).await;
    let seconds = started.elapsed().as_secs_f32();

    match outcome {
        Err(_elapsed) => vec![CheckItem::fail(
            MODEL,
            format!("the test classification did not finish within {limit:?}"),
            "the model may still be loading (try again), or raise [llm] timeout_seconds",
        )],
        Ok(Err(error)) => vec![CheckItem::fail(
            MODEL,
            format!("the test classification failed: {error}"),
            decide_error_hint(&error),
        )],
        Ok(Ok(verdict)) => {
            let mode = decider.output_mode();
            let mut items = vec![CheckItem::ok(
                MODEL,
                format!(
                    "test classification worked in {seconds:.1} s (judged \"{}\" with confidence {:.2})",
                    verdict.category.as_str(),
                    verdict.confidence
                ),
            )];
            let message = format!("structured output mode: {}", mode.label());
            items.push(match mode {
                crate::decider::OutputMode::Plain => CheckItem::warn(
                    MODEL,
                    message,
                    "answers are less reliable without structured output; a server that supports response_format works better",
                ),
                _ => CheckItem::ok(MODEL, message),
            });
            items
        }
    }
}

/// What to try for each kind of model failure.
pub fn decide_error_hint(error: &DecideError) -> String {
    match error {
        DecideError::Transport(_) => {
            String::from("is the model server running, and is [llm] base_url right?")
        }
        DecideError::Status {
            status: 401 | 403, ..
        } => String::from("set [llm] api_key to the key your server expects"),
        DecideError::Status { status: 404, .. } => String::from(
            "the endpoint or the model was not found: check [llm] base_url (it usually ends in /v1) and [llm] model",
        ),
        DecideError::Status { .. } => String::from("see the model server's log for the reason"),
        DecideError::MalformedResponse(_) => String::from(
            "this does not look like an OpenAI-compatible /chat/completions endpoint: check [llm] base_url",
        ),
        DecideError::InvalidVerdict(_) => String::from(
            "the model did not return the expected JSON: try a larger model, or raise [llm] max_tokens (reasoning models need room to think)",
        ),
        DecideError::ShuttingDown => String::from("this should not happen; please report it"),
    }
}

// ---------------------------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------------------------

/// The report area for an account.
fn account_area(name: &str) -> String {
    format!("Account {name}")
}

/// Runs all checks for one account: password, login, capabilities, inbox, Sent folder.
pub async fn probe_account(account: &AccountConfig) -> Vec<CheckItem> {
    let area = account_area(&account.name);

    // Reading the password may run the user's `password_command` (for example the macOS
    // Keychain). The value itself is never printed, only whether we got one.
    let password = match account.password() {
        Ok(password) => password,
        Err(error) => {
            return vec![CheckItem::fail(
                &area,
                format!("cannot get the password: {error}"),
                format!(
                    "see config.example.toml for storing the password in the macOS Keychain, or export {}",
                    password_env_var(&account.name)
                ),
            )];
        }
    };
    let mut items = vec![CheckItem::ok(&area, "password found (not shown)")];
    items.extend(
        probe_imap(
            &area,
            &account.imap_settings(&password),
            &account.inbox,
            &account.sent_folder,
        )
        .await,
    );
    items
}

/// The IMAP part of the account checks, separate so a test can run it against a test server
/// without a config file.
///
/// `configured_sent` is the `sent_folder` setting; empty means "detect it".
pub async fn probe_imap(
    area: &str,
    settings: &ImapSettings,
    inbox: &str,
    configured_sent: &str,
) -> Vec<CheckItem> {
    let mut items = Vec::new();

    let mut session = match ImapSession::connect(settings).await {
        Ok(session) => session,
        Err(error) => {
            items.push(CheckItem::fail(
                area,
                format!(
                    "cannot connect to {}:{} and log in: {error}",
                    settings.host, settings.port
                ),
                connect_hint(&error),
            ));
            return items;
        }
    };
    items.push(CheckItem::ok(
        area,
        format!(
            "connected to {}:{}, TLS ok, logged in as {}",
            settings.host, settings.port, settings.username
        ),
    ));

    match step("reading capabilities", session.capabilities()).await {
        Ok(caps) => items.push(capability_item(area, &caps)),
        Err(message) => items.push(CheckItem::fail(area, message, RETRY_HINT)),
    }

    match step(&format!("selecting {inbox}"), session.select(inbox)).await {
        Ok(info) => items.extend(mailbox_items(area, inbox, &info)),
        Err(message) => items.push(CheckItem::fail(
            area,
            message,
            "check the account's `inbox` setting; it is normally INBOX",
        )),
    }

    match step("listing folders", session.resolve_folders()).await {
        Ok(folders) => items.push(sent_item(area, configured_sent, &folders)),
        Err(message) => items.push(CheckItem::fail(area, message, RETRY_HINT)),
    }

    // A configured Sent folder is taken at its word by the service, so prove it can be
    // opened. SELECT changes nothing on the server.
    if !configured_sent.is_empty() {
        let what = format!("selecting the configured Sent folder {configured_sent}");
        match step(&what, session.select(configured_sent)).await {
            Ok(_) => items.push(CheckItem::ok(
                area,
                format!("configured Sent folder {configured_sent} can be opened"),
            )),
            Err(message) => items.push(CheckItem::fail(
                area,
                message,
                "fix `sent_folder`, or leave it empty to detect the folder automatically",
            )),
        }
    }

    // Be polite to the server. A failed logout does not matter for a read-only check.
    let _ = step("logging out", session.logout()).await;
    items
}

const RETRY_HINT: &str = "run `check` again; if it keeps failing, see the server's log";

/// Runs one IMAP command with a time limit and turns every failure into a message that
/// starts with what we were doing.
///
/// The generic `T` is whatever the command returns, so the same helper wraps `capabilities`,
/// `select` and the others. `impl Future` takes any future without boxing it.
async fn step<T>(
    what: &str,
    command: impl Future<Output = Result<T, ImapError>>,
) -> Result<T, String> {
    match tokio::time::timeout(IMAP_STEP_TIMEOUT, command).await {
        Err(_elapsed) => Err(format!("{what} timed out after {IMAP_STEP_TIMEOUT:?}")),
        Ok(Err(error)) => Err(format!("{what} failed: {error}")),
        Ok(Ok(value)) => Ok(value),
    }
}

/// What to try for each way connecting can fail.
pub fn connect_hint(error: &ImapError) -> String {
    match error {
        ImapError::Login(_) => String::from(
            "check the username and password; many providers need an app-specific password (iCloud always does)",
        ),
        ImapError::Timeout => String::from(
            "check the host and port, your network and any firewall; Tls vs StartTls must match the port",
        ),
        ImapError::Io(_) => String::from(
            "check `host` and `port`, and that the server is reachable from this machine",
        ),
        ImapError::Tls(_) | ImapError::NoRootCertificates | ImapError::InvalidServerName(_) => {
            String::from(
                "the TLS handshake failed: check `tls` and `port` (993 for \"tls\", 143 for \"starttls\"); a server with a private CA needs that CA trusted in the system keychain",
            )
        }
        ImapError::StartTlsUnsupported => String::from(
            "this server cannot upgrade to TLS on this port: try tls = \"tls\" with port 993",
        ),
        ImapError::InsecureTransport { .. } => String::from(
            "tls = \"none\" is only allowed for localhost; use \"tls\" or \"starttls\"",
        ),
        _ => String::from(RETRY_HINT),
    }
}

/// The server's capabilities, shown for information. Only the lack of IDLE is worth a warning.
pub fn capability_item(area: &str, caps: &ServerCapabilities) -> CheckItem {
    let yes_no = |on: bool| if on { "yes" } else { "no" };
    let message = format!(
        "capabilities: IDLE {}, UIDPLUS {}, MOVE {}",
        yes_no(caps.idle),
        yes_no(caps.uidplus),
        yes_no(caps.move_ext)
    );
    if caps.idle {
        CheckItem::ok(area, message)
    } else {
        CheckItem::warn(
            area,
            message,
            "without IDLE new mail is found by polling every 60 seconds instead of at once; everything still works",
        )
    }
}

/// The items for a selected inbox: size and `UIDVALIDITY`, and whether keywords are allowed.
pub fn mailbox_items(area: &str, folder: &str, info: &crate::imap::MailboxInfo) -> Vec<CheckItem> {
    let selected = CheckItem::ok(
        area,
        format!(
            "{folder} selected: {} messages, UIDVALIDITY {}",
            info.exists, info.uid_validity
        ),
    );
    let keywords = if info.keywords_allowed {
        CheckItem::ok(
            area,
            "custom keywords allowed (PERMANENTFLAGS has \\*), so flag colours can be stored",
        )
    } else {
        CheckItem::fail(
            area,
            "this mailbox does not allow custom keywords (PERMANENTFLAGS lacks \\*)",
            "flag colours are stored as keywords such as $MailFlagBit0, so they cannot work on this server or folder; \
             use another provider or server setting, or leave this account out of the config",
        )
    };
    vec![selected, keywords]
}

/// The Sent folder item: the configured one, the detected one, or a warning that there is none.
pub fn sent_item(area: &str, configured: &str, resolved: &ResolvedFolders) -> CheckItem {
    if !configured.is_empty() {
        return CheckItem::ok(area, format!("Sent folder (configured): {configured}"));
    }
    match &resolved.sent {
        Some(sent) => CheckItem::ok(area, format!("Sent folder (detected): {sent}")),
        None => CheckItem::warn(
            area,
            "no Sent folder found",
            "set `sent_folder` for this account; without it known senders are not learned and \"reply needed\" flags are not cleared after you answer",
        ),
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::imap::MailboxInfo;

    fn llm_config(server: &MockServer) -> LlmConfig {
        LlmConfig {
            base_url: format!("{}/v1", server.uri()),
            model: String::from("test-model"),
            timeout_seconds: 5,
            ..LlmConfig::default()
        }
    }

    fn statuses(items: &[CheckItem]) -> Vec<Status> {
        items.iter().map(|i| i.status).collect()
    }

    fn models_body(ids: &[&str]) -> Value {
        json!({"object": "list", "data": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()})
    }

    fn verdict_body() -> Value {
        let verdict = json!({
            "category": "marketing", "needs_reply": false, "deadline": null,
            "payment": "not_applicable", "confidence": 0.93, "reason": "A sale announcement."
        });
        json!({"choices": [{"message": {"content": verdict.to_string()}}]})
    }

    async fn mount_models(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    async fn mount_chat(server: &MockServer, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    // --- model probes against a fake server ---

    #[tokio::test]
    async fn healthy_model_passes_every_check() {
        let server = MockServer::start().await;
        mount_models(
            &server,
            ResponseTemplate::new(200).set_body_json(models_body(&["test-model"])),
        )
        .await;
        mount_chat(
            &server,
            ResponseTemplate::new(200).set_body_json(verdict_body()),
        )
        .await;

        let items = probe_model(&llm_config(&server)).await;
        assert!(items.iter().all(|i| i.status == Status::Ok), "{items:#?}");
        let text = format_items(&items);
        assert!(text.contains("model `test-model` is available"), "{text}");
        assert!(text.contains("marketing"), "{text}");
        assert!(text.contains("json_schema"), "{text}");
    }

    #[tokio::test]
    async fn missing_model_fails_and_names_the_alternatives() {
        let server = MockServer::start().await;
        mount_models(
            &server,
            ResponseTemplate::new(200).set_body_json(models_body(&["llama3:latest", "qwen3:14b"])),
        )
        .await;
        mount_chat(
            &server,
            ResponseTemplate::new(200).set_body_json(verdict_body()),
        )
        .await;

        let items = probe_model(&llm_config(&server)).await;
        let missing = items
            .iter()
            .find(|i| i.status == Status::Fail)
            .expect("a failing item");
        assert!(missing.message.contains("test-model"));
        let hint = missing.hint.as_deref().unwrap();
        assert!(
            hint.contains("llama3:latest") && hint.contains("qwen3:14b"),
            "{hint}"
        );
    }

    #[tokio::test]
    async fn listing_not_supported_is_only_a_warning() {
        let server = MockServer::start().await;
        mount_models(&server, ResponseTemplate::new(404)).await;
        mount_chat(
            &server,
            ResponseTemplate::new(200).set_body_json(verdict_body()),
        )
        .await;

        let items = probe_model(&llm_config(&server)).await;
        assert!(
            !items.iter().any(|i| i.status == Status::Fail),
            "{items:#?}"
        );
        assert!(items.iter().any(|i| i.status == Status::Warn));
        // The test classification still ran and passed.
        assert!(format_items(&items).contains("test classification worked"));
    }

    #[tokio::test]
    async fn garbage_answers_are_reported_not_trusted() {
        let server = MockServer::start().await;
        mount_models(
            &server,
            ResponseTemplate::new(200).set_body_string("<html>hi</html>"),
        )
        .await;
        mount_chat(
            &server,
            ResponseTemplate::new(200).set_body_string("not json"),
        )
        .await;

        let items = probe_model(&llm_config(&server)).await;
        // Unparseable list: a warning. Unparseable chat answer: a failure.
        assert_eq!(items[1].status, Status::Warn, "{items:#?}");
        let last = items.last().unwrap();
        assert_eq!(last.status, Status::Fail, "{items:#?}");
        assert!(last.message.contains("test classification failed"));
    }

    #[tokio::test]
    async fn unreachable_server_fails_once_and_skips_the_rest() {
        // Port 1 on localhost is refused immediately on every platform.
        let config = LlmConfig {
            base_url: String::from("http://127.0.0.1:1/v1"),
            timeout_seconds: 5,
            ..LlmConfig::default()
        };
        let items = probe_model(&config).await;
        assert_eq!(statuses(&items), [Status::Fail]);
        assert!(items[0].message.contains("cannot reach"));
    }

    #[tokio::test]
    async fn rejected_api_key_is_a_failure_with_a_hint() {
        let server = MockServer::start().await;
        mount_models(&server, ResponseTemplate::new(401)).await;
        mount_chat(&server, ResponseTemplate::new(401).set_body_string("no")).await;

        let items = probe_model(&llm_config(&server)).await;
        let fails: Vec<_> = items.iter().filter(|i| i.status == Status::Fail).collect();
        assert!(fails.len() >= 2, "{items:#?}");
        assert!(
            fails
                .iter()
                .all(|i| i.hint.as_deref().unwrap().contains("api_key"))
        );
    }

    #[tokio::test]
    async fn output_mode_fallback_is_reported_as_the_mode_that_works() {
        let server = MockServer::start().await;
        mount_models(
            &server,
            ResponseTemplate::new(200).set_body_json(models_body(&["test-model"])),
        )
        .await;
        // A server that rejects `response_format` entirely: the decider falls back to plain.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(|request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                if body.get("response_format").is_some() {
                    ResponseTemplate::new(400).set_body_string("unsupported")
                } else {
                    ResponseTemplate::new(200).set_body_json(verdict_body())
                }
            })
            .mount(&server)
            .await;

        let items = probe_model(&llm_config(&server)).await;
        let mode = items.last().unwrap();
        assert_eq!(mode.status, Status::Warn, "{items:#?}");
        assert!(mode.message.contains("plain text"));
    }

    // --- pure mapping ---

    #[test]
    fn model_names_match_with_or_without_latest() {
        let ids = vec![String::from("llama3:latest")];
        assert_eq!(model_presence_item("llama3", &ids).status, Status::Ok);
        let ids = vec![String::from("llama3")];
        assert_eq!(
            model_presence_item("llama3:latest", &ids).status,
            Status::Ok
        );
        assert_eq!(model_presence_item("llama2", &ids).status, Status::Fail);
        assert_eq!(model_presence_item("x", &[]).status, Status::Fail);
    }

    #[test]
    fn long_model_lists_are_cut() {
        let ids: Vec<String> = (0..12).map(|n| format!("m{n}")).collect();
        let item = model_presence_item("other", &ids);
        let hint = item.hint.unwrap();
        assert!(hint.contains("m7") && !hint.contains("m8"), "{hint}");
        assert!(hint.contains("and 4 more"), "{hint}");
    }

    #[test]
    fn missing_keywords_support_is_a_failure_that_explains_why() {
        let mut info = MailboxInfo {
            uid_validity: 7,
            uid_next: 10,
            exists: 9,
            permanent_flags: vec![String::from("\\Seen")],
            keywords_allowed: false,
        };
        let items = mailbox_items("Account a", "INBOX", &info);
        assert_eq!(statuses(&items), [Status::Ok, Status::Fail]);
        assert!(
            items[0].message.contains("9 messages") && items[0].message.contains("UIDVALIDITY 7")
        );
        assert!(items[1].hint.as_deref().unwrap().contains("flag colours"));

        info.keywords_allowed = true;
        let items = mailbox_items("Account a", "INBOX", &info);
        assert_eq!(statuses(&items), [Status::Ok, Status::Ok]);
    }

    #[test]
    fn missing_idle_warns_and_other_capabilities_are_info() {
        let caps = ServerCapabilities::from_atoms(["IMAP4rev1", "UIDPLUS"]);
        let item = capability_item("a", &caps);
        assert_eq!(item.status, Status::Warn);
        assert!(item.message.contains("IDLE no, UIDPLUS yes, MOVE no"));

        let caps = ServerCapabilities::from_atoms(["IDLE", "MOVE"]);
        assert_eq!(capability_item("a", &caps).status, Status::Ok);
    }

    #[test]
    fn sent_folder_is_configured_detected_or_a_warning() {
        let mut folders = ResolvedFolders {
            inbox: String::from("INBOX"),
            sent: Some(String::from("Sent Items")),
            delimiter: None,
            prefix: String::new(),
        };
        assert!(
            sent_item("a", "", &folders)
                .message
                .contains("detected): Sent Items")
        );
        assert!(
            sent_item("a", "Mine", &folders)
                .message
                .contains("configured): Mine")
        );
        folders.sent = None;
        let item = sent_item("a", "", &folders);
        assert_eq!(item.status, Status::Warn);
        assert!(item.hint.unwrap().contains("known senders"));
    }

    #[test]
    fn every_connect_error_gets_a_specific_hint() {
        assert!(connect_hint(&ImapError::Timeout).contains("firewall"));
        assert!(connect_hint(&ImapError::Tls(String::from("x"))).contains("993"));
        assert!(connect_hint(&ImapError::StartTlsUnsupported).contains("993"));
        assert!(
            connect_hint(&ImapError::InsecureTransport {
                host: String::from("h")
            })
            .contains("localhost")
        );
    }

    #[test]
    fn dry_run_off_is_a_warning() {
        assert_eq!(dry_run_item(true).status, Status::Ok);
        let item = dry_run_item(false);
        assert_eq!(item.status, Status::Warn);
        assert!(item.message.contains("flags will be written"));
    }

    #[test]
    fn database_item_opens_a_new_file_and_fails_on_a_bad_path() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("sub").join("state.db");
        assert_eq!(database_item(&good).status, Status::Ok);
        assert!(good.exists());

        // A file where a directory is needed.
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let bad = database_item(&blocker.join("state.db"));
        assert_eq!(bad.status, Status::Fail);
        assert!(bad.hint.unwrap().contains("db_path"));
    }

    // --- the report ---

    fn sample_report() -> Report {
        Report {
            items: vec![
                CheckItem::ok("Config", "loads"),
                CheckItem::warn("Config", "dry-run is off", "turn it on"),
                CheckItem::fail("Account a", "no keywords", "use another server"),
            ],
        }
    }

    #[test]
    fn text_groups_by_area_and_shows_hints_only_for_problems() {
        let expected = "\
Config
  [OK]   loads
  [WARN] dry-run is off
         -> turn it on

Account a
  [FAIL] no keywords
         -> use another server
";
        assert_eq!(sample_report().to_text(), expected);
    }

    #[test]
    fn summary_counts_failures_and_warnings() {
        assert_eq!(sample_report().summary(), "1 check failed, 1 warning.");
        assert_eq!(Report::default().summary(), "All checks passed.");
        let warned = Report {
            items: vec![
                CheckItem::warn("a", "m", "h"),
                CheckItem::warn("a", "m", "h"),
            ],
        };
        assert_eq!(warned.summary(), "All checks passed, with 2 warnings.");
        assert!(!warned.has_failures());
        assert!(sample_report().has_failures());
    }

    #[test]
    fn json_has_a_verdict_and_lowercase_statuses() {
        let value = sample_report().to_json();
        assert_eq!(value["ok"], false);
        assert_eq!(value["failed"], 1);
        assert_eq!(value["warnings"], 1);
        assert_eq!(value["items"][0]["status"], "ok");
        assert_eq!(value["items"][2]["hint"], "use another server");
        assert_eq!(value["items"][0]["hint"], Value::Null);
    }
}
