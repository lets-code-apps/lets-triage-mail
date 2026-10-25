//! Deciders: the part of the pipeline that turns what we know about a message into a
//! [`Verdict`].
//!
//! [`Decider`] is the first trait we define ourselves. A trait is a set of methods a type
//! promises to have, like an interface in Java or Swift. Today the only implementation is
//! [`LlmDecider`] (a local model behind an OpenAI-compatible HTTP endpoint). Later a small
//! decision model such as Laya can be added as a second implementation without touching the
//! callers (see `docs/CLASSIFIER.md`).
//!
//! Nothing here depends on message parsing or config: [`Evidence`] and [`LlmSettings`] are our
//! own plain structs, and the glue code converts into them.

mod llm;

use std::future::Future;

use crate::engine::Verdict;

pub use llm::{LlmDecider, LlmSettings, OutputMode, install_tls_provider};

/// The outcome of one SPF, DKIM or DMARC check, as reported by our own mail server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthResult {
    Pass,
    Fail,
    /// No result, or anything other than a clear pass or fail (`none`, `neutral`, ...).
    Unknown,
}

impl AuthResult {
    /// The word shown to the model.
    fn as_str(self) -> &'static str {
        match self {
            AuthResult::Pass => "pass",
            AuthResult::Fail => "fail",
            AuthResult::Unknown => "unknown",
        }
    }
}

/// Everything the prompt needs to know about one message, and nothing more.
///
/// The text fields (`from_name`, `subject`, `body_text`, ...) come from the sender and are
/// **untrusted**. The other fields are computed locally and are trustworthy. The prompt
/// keeps the two apart.
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    // Untrusted: written by the sender.
    pub from_address: String,
    pub from_name: String,
    pub reply_to: Option<String>,
    pub subject: String,
    /// The `Date` header as text; the model only reads it.
    pub date: Option<String>,
    /// Plain-text body (HTML already flattened). Never stored; only sent to the local model.
    pub body_text: String,
    /// True when the message was too large and only headers were fetched.
    pub headers_only: bool,
    pub attachment_names: Vec<String>,

    // Trustworthy: computed locally.
    pub spf: AuthResult,
    pub dkim: AuthResult,
    pub dmarc: AuthResult,
    /// `List-Unsubscribe`, `List-Id` or `Precedence: bulk` present.
    pub bulk_headers: bool,
    /// `Auto-Submitted` says a machine sent it.
    pub auto_submitted: bool,
    /// The user has written to this sender's domain before.
    pub known_domain: bool,
    /// The Reply-To domain differs from the From domain.
    pub reply_to_mismatch: bool,
    /// The display name contains an email address other than the real one.
    pub display_name_spoof: bool,
    pub link_count: usize,
}

/// Why a decision could not be made.
///
/// Every variant means "no verdict". Callers must treat an `Err` as "do nothing and retry
/// later", never as a default category (see the model-failure rule in `docs/PLAN.md`).
#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    /// The request failed before we got a response: connection refused, timeout, bad URL.
    // `#[from]` generates `impl From<reqwest::Error> for DecideError`, which is what lets the
    // `?` operator turn a reqwest error into ours automatically.
    #[error("could not reach the model: {0}")]
    Transport(#[from] reqwest::Error),

    /// The server answered with an error status.
    #[error("the model server answered HTTP {status}: {body}")]
    Status { status: u16, body: String },

    /// The server answered 200, but not with an OpenAI-style chat completion.
    #[error("unexpected response from the model server: {0}")]
    MalformedResponse(String),

    /// The model answered, but its answer is not a usable verdict.
    #[error("the model's answer is not a valid verdict: {0}")]
    InvalidVerdict(String),

    /// The concurrency limiter was shut down. Cannot happen today; kept so we never panic.
    #[error("the decider is shutting down")]
    ShuttingDown,
}

/// Something that can judge a message.
///
/// Written as `fn ... -> impl Future + Send` instead of `async fn` on purpose. A plain
/// `async fn` in a trait compiles, but callers cannot require the future to be `Send`, and
/// tokio's `spawn` needs that when account workers run on several threads. Implementations
/// may still write `async fn decide(...)`: the compiler checks that it matches.
///
/// Trade-off: traits with `async fn` (or `impl Future`) cannot be used as `dyn Decider`
/// without the `async-trait` crate, which boxes every future. We avoid it and use generics
/// instead (`fn run<D: Decider>(decider: D)`). A runtime choice between backends, such as a
/// cascade, becomes an enum or a generic struct over two deciders, not a trait object.
pub trait Decider {
    /// Judges one message. Any failure is an `Err`; there is no fallback verdict.
    fn decide(
        &self,
        evidence: &Evidence,
    ) -> impl Future<Output = Result<Verdict, DecideError>> + Send;
}
