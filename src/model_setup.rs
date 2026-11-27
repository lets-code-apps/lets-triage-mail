//! `model detect | pull | save`: the first-run setup commands (LET-137).
//!
//! The desktop onboarding asks what model servers are running, downloads a model, and
//! records the choice in the shared `model.toml` (see `docs/MODEL.md`). These are the three
//! engine commands it calls; the desktop itself never makes a network call
//! (see `docs/DESKTOP.md`).
//!
//! Privacy rule: a model endpoint must be on the local network — the same rule as `[llm]
//! base_url` (see `is_local_or_private_host` in `config.rs`). `detect --url` and `pull --url`
//! refuse anything else, and `save` refuses to record a remote url, so mail text can never
//! end up at a model server outside the user's own network.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::is_local_or_private_host;
use crate::decider::install_tls_provider;
use crate::shared_model;

/// Where Ollama listens by default.
pub const OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// Where LM Studio listens by default.
pub const LM_STUDIO_URL: &str = "http://127.0.0.1:1234";

/// The time limit for one `GET {url}/v1/models` probe. Both default probes run at the
/// same time, so a dead port costs the desktop 2 s, not 4 s.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// A model server to probe: the runtime name (as in [`shared_model::RUNTIMES`]) and its
/// base url, for example `ollama` at `http://127.0.0.1:11434`.
pub struct Endpoint {
    pub runtime: String,
    pub url: String,
}

/// The default probes: both servers a Mac user is likely to have running, checked at
/// once.
pub fn default_endpoints() -> Vec<Endpoint> {
    vec![
        Endpoint {
            runtime: "ollama".into(),
            url: OLLAMA_URL.into(),
        },
        Endpoint {
            runtime: "lmstudio".into(),
            url: LM_STUDIO_URL.into(),
        },
    ]
}

/// What one probe learned about one endpoint.
pub struct Found {
    pub runtime: String,
    pub url: String,
    pub reachable: bool,
    /// Installed model ids, as the server lists them.
    pub models: Vec<String>,
    /// The ids usable for classification: ids that do not contain `embed` (embedding
    /// models answer only similarity queries, never chat). The desktop needs both lists
    /// to tell "found, nothing usable" (only an embedding model installed) apart from
    /// "nothing installed".
    pub usable: Vec<String>,
    /// Why it is not reachable, when it is not.
    pub error: Option<String>,
}

/// The shared `model.toml` as `detect` reports it: only present when the file is
/// complete (url and model both set). `reachable` says whether that url answers now,
/// so the desktop can tell "the other app chose this and it works" from "the other
/// app chose it but its server is off".
pub struct SharedBlock {
    pub runtime: String,
    pub url: String,
    pub model: String,
    pub set_by: String,
    pub reachable: bool,
}

/// The result of `detect`: one entry per probe, in the order given, plus the shared
/// block when it exists. `detect` never fails: a server that is off the network is
/// simply `reachable = false` with its error text.
pub struct Detection {
    pub runtimes: Vec<Found>,
    pub shared: Option<SharedBlock>,
}

/// Probes every endpoint (`tokio::join!` for the two defaults, as `check` does) and
/// reads the shared file the caller passed in — never `$HOME` (the LET-135 rule:
/// paths come from the caller, so tests decide the path).
pub async fn detect(endpoints: Vec<Endpoint>, shared: shared_model::SharedModel) -> Detection {
    // The client constructor installs the TLS provider (see `probe_client`); no call needed
    // here. `pull` shares the same rule through `download_client`.
    let client = probe_client();

    let runtimes: Vec<Found> = match endpoints.len() {
        // One endpoint (a `--url` replacement): probe it alone.
        1 => vec![probe_endpoint(&client, &endpoints[0]).await],
        // Two defaults: start both probes, then wait for both with `tokio::join!`,
        // so the wall-clock cost is one timeout even if both ports are dead.
        2 => {
            let first = probe_endpoint(&client, &endpoints[0]);
            let second = probe_endpoint(&client, &endpoints[1]);
            let (older, newer) = tokio::join!(first, second);
            vec![older, newer]
        }
        // Not the production shape, but correct: probe them one after another.
        _ => {
            let mut items: Vec<Found> = vec![];
            for endpoint in endpoints {
                items.push(probe_endpoint(&client, &endpoint).await);
            }
            items
        }
    };

    // The shared block: probe the stored base url. A complete file gives one even when
    // the server is off, flagged by `reachable`.
    let shared_block: Option<SharedBlock> = match shared.chat_base_url() {
        None => None,
        Some(base_url) => {
            let reachable = probe_listing(&client, base_url).await.is_ok();
            Some(SharedBlock {
                runtime: shared.runtime.clone().unwrap_or_default(),
                url: shared.url.clone().unwrap_or_default(),
                // `chat_model` borrows from `shared`; this field owns a copy.
                model: shared.chat_model().unwrap_or("").to_string(),
                set_by: shared.set_by.clone().unwrap_or_default(),
                reachable,
            })
        }
    };
    Detection {
        runtimes,
        shared: shared_block,
    }
}

/// The HTTP client for the probes: no redirects (a redirect would follow the server's
/// `Location:` to a host outside the local-network rule, see `decider/llm.rs`) and a short
/// total timeout — one dead port costs the desktop 2 s, not 4 s.
///
/// Every `reqwest::Client` builder in this crate calls `install_tls_provider()` first: the crate
/// is built with `rustls-no-provider`, so reqwest panics when it builds a client and no crypto
/// provider is installed. Doing it inside the constructor (rather than only at each caller) is
/// what keeps a standalone `model pull` from panicking before it reaches the server.
pub fn probe_client() -> reqwest::Client {
    install_tls_provider();
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a basic HTTP client always builds")
}

/// The HTTP client for `model pull`: same no-redirect rule, but **no total timeout**.
///
/// A model download is a single response whose body streams for minutes (a multi-GB blob). The
/// probe's 2 s total timeout would cut that stream after two seconds, so a download must use
/// a client without it. A short `connect_timeout` is kept so a dead port still fails fast.
pub fn download_client() -> reqwest::Client {
    install_tls_provider();
    reqwest::Client::builder()
        .connect_timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("a basic HTTP client always builds")
}

/// One endpoint: `GET {url}/v1/models`. Both Ollama and LM Studio answer that OpenAI-style
/// path; any HTTP answer proves reachability, but only a parseable list gives model ids.
async fn probe_endpoint(client: &reqwest::Client, endpoint: &Endpoint) -> Found {
    match probe_listing(client, endpoint.url.clone()).await {
        Err(error) => Found {
            runtime: endpoint.runtime.clone(),
            url: endpoint.url.clone(),
            reachable: false,
            models: [].to_vec(),
            usable: [].to_vec(),
            error: Some(error),
        },
        Ok((status, body)) => {
            let (models, usable) = parse_model_list(status, body);
            Found {
                runtime: endpoint.runtime.clone(),
                url: endpoint.url.clone(),
                reachable: true,
                models,
                usable,
                error: None,
            }
        }
    }
}

/// `GET {url}/v1/models` under the timeout: the status and body, or the error text.
/// `fetch_models` in `check.rs` does the same for `[llm] base_url`.
async fn probe_listing(client: &reqwest::Client, url: String) -> Result<(u16, String), String> {
    let response = match client.get(models_url(url)).send().await {
        Err(error) => return Err(describe_error(error)),
        Ok(response) => response,
    };
    let status = response.status().as_u16();
    // A body that cannot be read counts as empty; the status is what proves reachability.
    let body = response.text().await.unwrap_or_default();
    Ok((status, body))
}

/// `{url}/v1/models`, unless the url already names `/v1` (see `chat_base_url` in
/// `shared_model.rs`).
fn models_url(url: String) -> String {
    let base = url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

/// The shape of an OpenAI `GET /models` answer; only the ids matter (see `ModelList`
/// in `check.rs`).
#[derive(Deserialize)]
struct Listing {
    data: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    id: String,
}

/// Ids from a 2xx listing; a body that does not parse is "reachable, no list" (some
/// servers cannot list models, and chat may still work — `check.rs` warns the same way).
fn parse_model_list(status: u16, body: String) -> (Vec<String>, Vec<String>) {
    let ids: Vec<String> = match status {
        200..=299 => match serde_json::from_str::<Listing>(&body) {
            Err(_) => [].to_vec(),
            Ok(list) => list.data.into_iter().map(|entry| entry.id).collect(),
        },
        _ => [].to_vec(),
    };
    // Embedding models are listed but cannot answer chat; the desktop needs both lists
    // to tell "found, nothing usable" from "nothing installed".
    let usable = ids
        .clone()
        .into_iter()
        .filter(|id| !id.contains("embed"))
        .collect();
    (ids, usable)
}

/// reqwest's `Display` for a refused connection is just "error sending request"; the
/// useful cause sits at the end of the `source` chain (see `describe_reqwest_error`
/// in `check.rs`).
fn describe_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        return "timed out".to_string();
    }
    let mut root: &dyn std::error::Error = &error;
    while let Some(cause) = root.source() {
        root = cause;
    }
    root.to_string()
}

// ------------------------------------------------------------------- pull

/// `model pull`: asks Ollama to download a model. Ollama's native endpoint is
/// `POST {url}/api/pull` with body `{"model": name}`; its answer is NDJSON — one JSON
/// object per line (`pulling manifest`, `downloading`, `verifying`, `writing manifest`,
/// `success`, or `error`). A non-2xx request (LM Studio answers 404: it downloads
/// through its own GUI, not this endpoint) returns one sentence, which the CLI prints
/// as an error line and exits 1 on.
///
/// Errors inside the stream are *lines*, not thrown: the stream ends with
/// `{"stage": "error", ...}` and the CLI exits 1 (the desktop reads lines until the
/// stream ends, then shows the failure).
pub async fn start_pull(
    client: reqwest::Client,
    url: String,
    model: String,
) -> Result<reqwest::Response, String> {
    let body = json!({"model": model}).to_string();
    let request = client
        .post(format!("{}/api/pull", url.trim_end_matches('/')))
        .body(body);
    let response = match request.send().await {
        Err(error) => return Err(describe_error(error)),
        Ok(response) => response,
    };
    if response.status().is_success() {
        return Ok(response);
    }
    // The server's own sentence if it gave one (Ollama's 404 names the model), else the
    // status number. `text` consumes the response body, so read the status first.
    let code = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    let why = text.trim();
    if !why.is_empty() {
        return Err(why.to_string());
    }
    Err(format!("the model server answered HTTP {code}",))
}

/// Splits `buffer` on `\n`, returning the complete lines and the unfinished tail. A
/// chunk of the raw byte stream never lines up with a line: one line can span two
/// chunks and one chunk can hold several lines, so the caller feeds the returned tail
/// back in with the next chunk. Taking and returning the buffer by value keeps that
/// ownership hand-off explicit (no borrow of a buffer that is about to be replaced).
/// The split is at an ASCII `\n`, so cutting UTF-8 text on it is safe.
pub fn drain_lines(buffer: Vec<u8>) -> (Vec<String>, Vec<u8>) {
    let mut complete: Vec<String> = vec![];
    // `start` is the first byte of the line being scanned; it advances past each `\n`.
    let mut start = 0;
    for (index, byte) in buffer.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        // `start..index` is one whole line (the `\n` at `index` is the delimiter, not
        // part of the line). Invalid UTF-8 from a hostile server decodes with replacement
        // characters, never a panic.
        complete.push(String::from_utf8_lossy(&buffer[start..index]).to_string());
        start = index + 1;
    }
    // Everything after the last newline is the tail for the next chunk.
    let tail = buffer[start..].to_vec();
    (complete, tail)
}

/// One NDJSON line in, one line out in the shape the desktop reads:
/// `{"stage": "pulling manifest"}`, `{"stage": "downloading", "total": N, "completed":
/// M}` when the server reports progress numbers, `{"stage": "success"}`,
/// `{"stage": "error", "error": "..."}`. A line that is not JSON becomes
/// `{"stage": "other", "status": "<raw text>"}` so nothing is silently dropped.
pub fn normalise_pull_line(raw: String) -> Value {
    let parsed = match serde_json::from_str::<Value>(&raw) {
        Err(_) => return json!({"stage": "other", "status": raw}),
        Ok(value) => value,
    };
    // An error line wins: it carries the reason the pull stopped.
    if let Some(value) = parsed.get("error")
        && let Some(error) = value.as_str()
    {
        return json!({"stage": "error", "error": error});
    }
    let status = match parsed.get("status") {
        Some(value) => value.as_str().unwrap_or(&raw),
        None => raw.as_str(),
    };
    // Ollama's download lines read "downloading <hash>: 43%"; one stage name for all
    // of them keeps the desktop's progress bar simple.
    let stage = if status.starts_with("downloading") {
        "downloading"
    } else {
        status
    };
    let mut line = json!({"stage": stage});
    // Progress numbers, when the server sends them: `total` and `completed` bytes.
    if let Some(value) = parsed.get("total")
        && value.is_number()
    {
        line["total"] = (*value).clone();
    }
    if let Some(value) = parsed.get("completed")
        && value.is_number()
    {
        line["completed"] = (*value).clone();
    }
    line
}

// ------------------------------------------------------------------- save

/// The outcome of reading a whole pull stream: the normalised lines the desktop shows, plus
/// three flags the CLI needs to decide whether the download actually finished.
///
/// A stream can end without ever reaching the `success` stage (the server closed the body
/// early, or a timeout cut it), and reading the body can itself fail mid-download. Treating
/// either as success would leave the desktop believing a partial download is a finished model,
/// so the caller checks these flags rather than trusting "the loop ended".
pub struct PullStream {
    pub lines: Vec<Value>,
    /// A `{"stage": "success"}` line arrived: the download is complete.
    pub saw_success: bool,
    /// A `{"stage": "error", ...}` line arrived: the server reported why it stopped.
    pub saw_error: bool,
    /// Reading the body failed (a dropped connection or a client timeout cut it mid-stream).
    pub read_failed: bool,
}

/// Reads a pull response's NDJSON body to the end, chunk by chunk, into [`PullStream`].
///
/// A chunk of the raw byte stream never lines up with a line, so `drain_lines` carries the
/// unfinished tail into the next chunk (see `drain_lines`). A body-read error sets
/// `read_failed` and stops the loop; the caller turns an incomplete or failed stream into a
/// non-zero exit (see `pull` in `cli/model.rs`).
pub async fn consume_pull_stream(mut response: reqwest::Response) -> PullStream {
    let mut lines: Vec<Value> = vec![];
    let mut saw_success = false;
    let mut saw_error = false;
    let mut read_failed = false;
    let mut buffer: Vec<u8> = vec![];

    loop {
        match response.chunk().await {
            Err(_) => {
                read_failed = true;
                break;
            }
            Ok(None) => break,
            Ok(Some(chunk)) => {
                buffer.extend(chunk);
                let (complete, tail) = drain_lines(buffer);
                buffer = tail;
                for line in complete {
                    let shaped = normalise_pull_line(line);
                    match shaped
                        .get("stage")
                        .and_then(|stage| stage.as_str())
                        .unwrap_or_default()
                    {
                        "success" => saw_success = true,
                        "error" => saw_error = true,
                        _ => {}
                    }
                    lines.push(shaped);
                }
            }
        }
    }

    // Anything after the last newline is a final, unterminated line.
    if !buffer.is_empty() {
        let shaped = normalise_pull_line(String::from_utf8_lossy(&buffer).to_string());
        match shaped
            .get("stage")
            .and_then(|stage| stage.as_str())
            .unwrap_or_default()
        {
            "success" => saw_success = true,
            "error" => saw_error = true,
            _ => {}
        }
        lines.push(shaped);
    }

    PullStream {
        lines,
        saw_success,
        saw_error,
        read_failed,
    }
}

/// Whether a finished pull stream must be treated as a failure (the CLI uses this to exit 1).
///
/// Three independent ways a pull has NOT finished: the body read failed (`read_failed`, e.g. a
/// connection dropped mid-download), the server reported an error stage, or the stream ended
/// without ever reaching the `success` stage. A loop merely ending is not proof of success.
pub fn pull_stream_failed(stream: PullStream) -> bool {
    stream.read_failed || stream.saw_error || !stream.saw_success
}

// ------------------------------------------------------------------- save

/// The host part of an `http(s)://host[:port]/path` url, for the local-only rule.
///
/// Uses `reqwest::Url` (as `config.rs` does for `llm.base_url` at `:749`): `host_str()`
/// returns the hostname *without* the port, which is the text `is_local_or_private_host`
/// understands (it parses `127.0.0.1` as an `IpAddr`; `127.0.0.1:11434` would not parse).
/// A url that is not `http`/`https`, or that has no host (e.g. `mailto:`), gives `None`.
pub fn host_of(url: &str) -> Option<String> {
    let parsed = match reqwest::Url::parse(url.trim()) {
        Err(_) => return None,
        Ok(parsed) => parsed,
    };
    // Only a page url counts; `mailto:` and friends have no host to check.
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return None;
    }
    // `host_str()` borrows from `parsed`; `map` copies the host into an owned `String`.
    parsed.host_str().map(|host| host.to_string())
}

/// Refuses a model url that is not on the local network, with a message naming the
/// url. Used by `model detect --url`, `model pull --url` and `model save --url`.
pub fn ensure_local(url: String) -> Result<String, String> {
    match host_of(url.as_str()) {
        None => Err(format!("`{url}` is not an http:// or https:// url")),
        Some(host) if is_local_or_private_host(host.as_str()) => Ok(url),
        Some(_host) => Err(format!(
            "`{url}` is not on the local network; model servers must be local so mail text never leaves your network"
        )),
    }
}

/// `model save`: records the choice in the shared `model.toml` through
/// [`shared_model::write`], which validates first (an unknown runtime, a blank model
/// or credentials in the url are refused before anything is written). The url goes
/// through [`ensure_local`] first: the privacy rule is enforced here, not by the
/// shared file's own rules (the other app has its own).
pub fn save(path: &Path, runtime: String, url: String, model: String) -> Result<(), String> {
    let local_url = ensure_local(url)?;
    let record = shared_model::SharedModel {
        runtime: Some(runtime),
        url: Some(local_url),
        model: Some(model),
        // Which app wrote the file last (see `shared_model::WRITERS`); `detect`
        // reports it so the desktop can say "the model is shared with RAG Docs".
        set_by: Some("triage mail".to_string()),
        updated: Some(jiff::Timestamp::now().to_string()),
    };
    match shared_model::write(path, &record) {
        Err(error) => Err(error.to_string()),
        Ok(_value) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn models_url_adds_v1_once() {
        assert_eq!(models_url("http://h:1".into()), "http://h:1/v1/models");
        assert_eq!(models_url("http://h:1/".into()), "http://h:1/v1/models");
        assert_eq!(models_url("http://h:1/v1".into()), "http://h:1/v1/models");
    }

    #[test]
    fn host_of_parses_urls_without_the_port() {
        // `host_str()` gives the host only; the port is not part of what `is_local_or_private_host`
        // parses as an `IpAddr`.
        assert_eq!(host_of("http://127.0.0.1:11434"), Some("127.0.0.1".into()));
        assert_eq!(host_of("https://a.b.c/path"), Some("a.b.c".into()));
        assert_eq!(host_of("ftp://x"), None, "not http/https");
        assert_eq!(host_of("127.0.0.1:11434"), None, "not a url (no scheme)");
    }

    #[test]
    fn ensure_local_refuses_remote_urls() {
        assert!(ensure_local("http://127.0.0.1:11434".into()).is_ok());
        assert!(ensure_local("http://localhost:1234".into()).is_ok());
        assert!(
            ensure_local("http://10.0.0.5:11434".into()).is_ok(),
            "private IPv4 range"
        );
        let reason = ensure_local("https://api.openai.com".into()).err().unwrap();
        assert!(reason.contains("local network"), "reason was: {reason}");
        assert!(ensure_local("not a url".into()).is_err());
    }

    #[test]
    fn embedding_filter_splits_usable_from_listed() {
        let body = r#"{"data": [{"id": "qwen3:4b"}, {"id": "nomic-embed-text"}]}"#;
        let (ids, usable) = parse_model_list(200, body.into());
        assert_eq!(ids, ["qwen3:4b", "nomic-embed-text"]);
        assert_eq!(
            usable,
            ["qwen3:4b"],
            "nomic-embed-text is an embedding model, not chat-usable"
        );
        let (ids, usable) = parse_model_list(404, "anything".into());
        assert!(ids.is_empty(), "a non-2xx answer lists nothing");
        assert!(usable.is_empty());
    }

    #[test]
    fn normalise_pull_line_maps_ollama_stages() {
        let line = normalise_pull_line(r#"{"status": "pulling manifest"}"#.into());
        assert_eq!(line, json!({"stage": "pulling manifest"}));
        let line = normalise_pull_line(
            r#"{"status": "downloading abc123: 43%", "total": 1000, "completed": 430}"#.into(),
        );
        assert_eq!(
            line,
            json!({"stage": "downloading", "total": 1000, "completed": 430})
        );
        let line = normalise_pull_line(r#"{"status": "success"}"#.into());
        assert_eq!(line, json!({"stage": "success"}));
        let line = normalise_pull_line(r#"{"error": "manifest not found"}"#.into());
        assert_eq!(line["stage"], "error");
        assert_eq!(line["error"], "manifest not found");
        let line = normalise_pull_line("not json at all".into());
        assert_eq!(line, json!({"stage": "other", "status": "not json at all"}));
    }

    #[test]
    fn drain_lines_carries_the_tail_across_chunks() {
        // One line split across two chunks: the first chunk completes nothing and
        // hands back the tail; the second completes the whole line plus another.
        let mut buffer: Vec<u8> = vec![];
        buffer.extend(b"{\"status\": \"pull");
        let (lines, tail) = drain_lines(buffer);
        buffer = tail;
        assert!(lines.is_empty(), "no complete line yet");
        assert!(
            !buffer.is_empty(),
            "the whole first chunk is carried as the tail"
        );
        buffer.extend(b"ing manifest\"}\nsecond\n");
        let (lines, tail) = drain_lines(buffer);
        assert_eq!(lines, [r#"{"status": "pulling manifest"}"#, "second"]);
        assert!(
            tail.is_empty(),
            "nothing unfinished after a trailing newline"
        );
    }

    #[tokio::test]
    async fn a_closed_port_is_not_found_fast() {
        // Port 1 refuses the connection at once. The whole detect must return inside
        // 3 s (the timeout wraps it, so a hang fails the test rather than freezing it).
        let endpoints = vec![Endpoint {
            runtime: "ollama".into(),
            url: "http://127.0.0.1:1".into(),
        }];
        let detection = tokio::time::timeout(
            Duration::from_secs(3),
            detect(endpoints, shared_model::SharedModel::default()),
        )
        .await;
        if detection.is_err() {
            panic!("detect did not return within 3 s");
        }
        let detection = detection.unwrap();
        assert_eq!(detection.runtimes.len(), 1);
        let found = &detection.runtimes[0];
        assert!(!found.reachable, "a closed port is not found");
        let reason = found.error.clone().unwrap_or_default();
        assert!(reason.contains("refused"), "the error says why: {reason}");
    }

    #[tokio::test]
    async fn detect_lists_models_and_usable_from_a_fake_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [
                {"id": "qwen3:4b"},
                {"id": "nomic-embed-text"},
            ]})))
            .mount(&server)
            .await;
        let endpoints = vec![Endpoint {
            runtime: "ollama".into(),
            url: server.uri(),
        }];
        let detection = detect(endpoints, shared_model::SharedModel::default()).await;
        assert_eq!(detection.runtimes.len(), 1);
        let found = &detection.runtimes[0];
        assert!(found.reachable);
        assert_eq!(found.models.len(), 2);
        assert_eq!(found.usable, ["qwen3:4b"]);
    }

    #[tokio::test]
    async fn the_shared_block_carries_reachable() {
        // A complete shared file whose url answers (the fake server, on the loopback
        // range) is reported with set_by and reachable.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": []})))
            .mount(&server)
            .await;
        let shared = shared_model::SharedModel {
            runtime: Some("ollama".into()),
            url: Some(server.uri()),
            model: Some("qwen3:8b".into()),
            set_by: Some("rag docs".into()),
            updated: None,
        };
        let detection = detect(default_endpoints(), shared).await;
        let block = detection.shared.expect("the shared block is reported");
        assert!(block.reachable);
        assert_eq!(block.set_by, "rag docs");
        assert_eq!(block.model, "qwen3:8b");
    }

    #[test]
    fn save_writes_the_shared_file_and_refuses_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.toml");
        assert!(
            save(
                &path,
                "ollama".into(),
                "http://127.0.0.1:11434".into(),
                "qwen3:4b".into()
            )
            .is_ok()
        );
        let record = shared_model::read(&path);
        assert_eq!(record.set_by, Some("triage mail".into()));
        assert_eq!(record.chat_model(), Some("qwen3:4b"));

        // Refusals leave nothing behind: a save that fails has written no file at all.
        let remote = dir.path().join("remote.toml");
        assert!(
            save(
                &remote,
                "ollama".into(),
                "https://api.openai.com".into(),
                "qwen3:4b".into()
            )
            .is_err()
        );
        assert!(
            !remote.exists(),
            "a remote url is refused before anything is written"
        );
        let bad = dir.path().join("bad.toml");
        assert!(
            save(
                &bad,
                "cloud".into(),
                "http://127.0.0.1:11434".into(),
                "qwen3:4b".into()
            )
            .is_err()
        );
        assert!(
            !bad.exists(),
            "an unknown runtime is refused (shared_model::validate)"
        );
        let cred = dir.path().join("cred.toml");
        assert!(
            save(
                &cred,
                "ollama".into(),
                "http://user:pw@127.0.0.1:11434".into(),
                "qwen3:4b".into()
            )
            .is_err()
        );
        assert!(!cred.exists(), "credentials in the url are refused");
    }

    #[tokio::test]
    async fn a_pull_stream_ends_in_normalised_lines() {
        // The fake Ollama: one NDJSON body. The CLI loop (chunk, drain_lines,
        // normalise_pull_line) turns it into the desktop's lines, error last.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"status":"pulling manifest"}
{"status":"downloading x: 1%","total":10,"completed":1}
{"error":"manifest not found"}
"#,
            ))
            .mount(&server)
            .await;
        let response = start_pull(probe_client(), server.uri(), "qwen3:4b".into()).await;
        if response.is_err() {
            panic!("the fake server should agree to pull");
        }
        let mut response = response.unwrap();
        let mut buffer: Vec<u8> = vec![];
        let mut lines: Vec<Value> = vec![];
        loop {
            match response.chunk().await {
                Err(_) => break,
                Ok(None) => break,
                Ok(Some(chunk)) => {
                    buffer.extend(chunk);
                    let (complete, tail) = drain_lines(buffer);
                    buffer = tail;
                    for line in complete {
                        lines.push(normalise_pull_line(line));
                    }
                }
            }
        }
        if !buffer.is_empty() {
            let tail = String::from_utf8_lossy(&buffer).to_string();
            lines.push(normalise_pull_line(tail));
        }
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["stage"], "pulling manifest");
        assert_eq!(lines[1]["stage"], "downloading");
        assert_eq!(lines[1]["total"], json!(10));
        assert_eq!(lines[2]["stage"], "error");
        assert_eq!(lines[2]["error"], "manifest not found");
    }

    #[tokio::test]
    async fn a_server_without_a_pull_endpoint_gives_one_sentence() {
        // LM Studio does not serve /api/pull: 404, and the CLI gets one sentence.
        let server = MockServer::start().await;
        let sentence = start_pull(probe_client(), server.uri(), "qwen3:4b".into()).await;
        let reason = sentence.expect_err("a 404 server gives a refusal");
        assert!(reason.contains("404"), "reason was: {reason}");
    }

    #[tokio::test]
    async fn consume_pull_stream_sees_a_success_stage() {
        // A complete pull: the stream reaches `success`, so the caller treats it as done.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"status":"pulling manifest"}
{"status":"downloading x: 50%","total":10,"completed":5}
{"status":"success"}
"#,
            ))
            .mount(&server)
            .await;
        let response = start_pull(download_client(), server.uri(), "qwen3:4b".into())
            .await
            .expect("the fake server agrees to pull");
        let stream = consume_pull_stream(response).await;
        assert!(stream.saw_success, "the success stage is recorded");
        assert!(!stream.saw_error, "no error line in a successful pull");
        assert!(!stream.read_failed, "the body read to the end");
        assert_eq!(stream.lines.len(), 3);
        assert_eq!(stream.lines[2]["stage"], "success");
    }

    #[tokio::test]
    async fn consume_pull_stream_marks_an_incomplete_body_as_not_done() {
        // A server that streams stages then stops cleanly but never sends `success`: not a
        // finished download. The caller must turn this into a non-zero exit (see the CLI test).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"status":"pulling manifest"}
{"status":"writing manifest"}
"#,
            ))
            .mount(&server)
            .await;
        let response = start_pull(download_client(), server.uri(), "qwen3:4b".into())
            .await
            .expect("the fake server agrees to pull");
        let stream = consume_pull_stream(response).await;
        assert!(!stream.saw_success, "no success stage arrived");
        assert!(
            !stream.saw_error,
            "the server never reported an error either"
        );
        assert!(!stream.read_failed, "the body did read to its end");
        assert_eq!(stream.lines.len(), 2);
    }

    #[test]
    fn pull_stream_failed_covers_every_failure_mode() {
        // The CLI exits 1 exactly when this is true. A clean success is the only `false`.
        let ok = PullStream {
            lines: vec![],
            saw_success: true,
            saw_error: false,
            read_failed: false,
        };
        assert!(!pull_stream_failed(ok), "a clean success is not a failure");
        let errored = PullStream {
            lines: vec![],
            saw_success: false,
            saw_error: true,
            read_failed: false,
        };
        assert!(pull_stream_failed(errored), "an error stage is a failure");
        let dropped = PullStream {
            lines: vec![],
            saw_success: false,
            saw_error: false,
            read_failed: true,
        };
        assert!(
            pull_stream_failed(dropped),
            "a body dropped mid-download is a failure"
        );
        let partial = PullStream {
            lines: vec![],
            saw_success: false,
            saw_error: false,
            read_failed: false,
        };
        assert!(
            pull_stream_failed(partial),
            "a stream that never reached success is a failure"
        );
    }

    #[tokio::test]
    async fn a_real_download_is_not_cut_at_two_seconds() {
        // Finding 5: a real model pull streams longer than the probe's 2 s. The download client has
        // no total timeout, so a 3 s body still arrives whole; the 2 s probe client would abort it.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/pull"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(3000))
                    .set_body_string(
                        r#"{"status":"downloading x: 50%","total":10,"completed":5}
{"status":"success"}
"#,
                    ),
            )
            .mount(&server)
            .await;
        let response = start_pull(download_client(), server.uri(), "qwen3:4b".into())
            .await
            .expect("a slow pull still connects");
        let stream = consume_pull_stream(response).await;
        assert!(stream.saw_success, "a slow download still reaches success");
        assert!(!stream.read_failed, "the body was not cut short");
        assert!(
            !pull_stream_failed(stream),
            "a completed slow download is not a failure"
        );
    }
}
