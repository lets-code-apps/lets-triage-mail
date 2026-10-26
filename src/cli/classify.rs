//! The `classify` command: judge `.eml` files with the local model, without touching any
//! mailbox or database.
//!
//! This is how the prompt gets tuned (milestone A in `docs/PLAN.md`): export a few real
//! messages, run them through here, and read the verdicts.
//!
//! The command logic is generic over [`Decider`], so the tests can run it against a fake
//! model server, and later a different decider can be dropped in.

use std::path::{Path, PathBuf};

use anyhow::Context;
use lets_triage_mail::config::Config;
use lets_triage_mail::decider::{Decider, Evidence, LlmDecider, LlmSettings};
use lets_triage_mail::engine::{FlagPolicy, Trust, Verdict, choose_flag, screen_sender};
use lets_triage_mail::message::{
    HEADERS_ONLY_THRESHOLD, ParsedMessage, ProviderHeaders, parse_headers_only, parse_message,
};
use lets_triage_mail::precheck::{SOURCE_MODEL, SOURCE_RULE, precheck};
use serde_json::{Value, json};

/// Classifies each file and prints the result. One bad file does not stop the others, but
/// makes the whole command fail at the end (non-zero exit code).
///
/// `account` names the account the files came from, so that its `provider_headers` apply.
pub async fn run(
    config_path: &Path,
    files: &[PathBuf],
    json: bool,
    account: Option<&str>,
) -> anyhow::Result<()> {
    let config = super::load_config(config_path)?;
    let provider = provider_for(&config, account)?;
    let decider = LlmDecider::new(LlmSettings::from(&config.llm))
        .context("could not set up the model client")?;

    let mut failed = 0;
    for file in files {
        match classify_file(&decider, &config, provider, file).await {
            Ok(outcome) if json => println!("{}", outcome.to_json()),
            Ok(outcome) => println!("{}", outcome.render()),
            Err(error) => {
                failed += 1;
                // `{error:#}` prints the whole chain: "reading x.eml: No such file ...".
                eprintln!("{}: error: {error:#}", file.display());
                if json {
                    let failure = json!({
                        "file": file.display().to_string(),
                        "error": format!("{error:#}"),
                    });
                    println!("{failure}");
                }
            }
        }
    }

    match failed {
        0 => Ok(()),
        n => anyhow::bail!("{n} of {} files could not be classified", files.len()),
    }
}

/// Everything `classify` knows about one file, ready to print. It holds the verdict and
/// header fields only: never the body.
#[derive(Debug)]
struct Outcome {
    file: PathBuf,
    from: String,
    subject: String,
    verdict: Verdict,
    /// Who decided: `"rule"` (the pre-check) or `"model"`.
    source: &'static str,
    /// The chosen flag with its display name and colour, if any.
    flag: Option<ChosenFlag>,
    /// Why there is no flag; `Some` exactly when `flag` is `None`.
    no_flag_reason: Option<String>,
}

#[derive(Debug)]
struct ChosenFlag {
    name: String,
    colour: String,
}

/// The provider headers of the named account; none without a name.
fn provider_for(config: &Config, account: Option<&str>) -> anyhow::Result<Option<ProviderHeaders>> {
    let Some(name) = account else {
        return Ok(None);
    };
    match config.account(name) {
        Some(account) => Ok(account.provider_headers),
        None => {
            let known: Vec<&str> = config.accounts.iter().map(|a| a.name.as_str()).collect();
            anyhow::bail!(
                "no account named {name:?} in the config (accounts: {})",
                known.join(", ")
            )
        }
    }
}

async fn classify_file<D: Decider>(
    decider: &D,
    config: &Config,
    provider: Option<ProviderHeaders>,
    file: &Path,
) -> anyhow::Result<Outcome> {
    let raw = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let authserv_id = Some(config.policy.authserv_id.as_str()).filter(|id| !id.is_empty());
    let message = if raw.len() > HEADERS_ONLY_THRESHOLD {
        // Big messages are mostly attachments; the headers are enough to judge them.
        parse_headers_only(&raw, authserv_id, provider)
    } else {
        parse_message(&raw, authserv_id, provider)
    }
    .with_context(|| format!("parsing {}", file.display()))?;

    // No database here, so we know nothing about the sender: not a known domain, no rule.
    let evidence = Evidence::from_message(&message, false);
    // Same order as the worker: certain spam is decided by code, and only the rest is
    // shown to the model.
    let (verdict, source) = match precheck(&evidence) {
        Some(verdict) => (verdict, SOURCE_RULE),
        None => (decider.decide(&evidence).await?, SOURCE_MODEL),
    };

    let policy = config.flag_policy();
    let trust = screen_sender(None, false, message.auth.any_fail());
    let chosen = choose_flag(trust, &verdict, &policy);

    let (flag, no_flag_reason) = match chosen {
        Some(flag) => (
            Some(ChosenFlag {
                name: config.flag_name(flag).to_string(),
                colour: format!("{:?}", policy.colour_for(flag)).to_lowercase(),
            }),
            None,
        ),
        None => (None, Some(explain_no_flag(trust, &verdict, &policy))),
    };

    Ok(Outcome {
        file: file.to_path_buf(),
        from: sender_text(&message),
        subject: message.subject.clone(),
        verdict,
        source,
        flag,
        no_flag_reason,
    })
}

fn sender_text(message: &ParsedMessage) -> String {
    if message.from_name.is_empty() {
        message.from_address.clone()
    } else {
        format!("{} <{}>", message.from_name, message.from_address)
    }
}

/// Says in words why `choose_flag` returned nothing, mirroring its checks in order.
fn explain_no_flag(trust: Trust, verdict: &Verdict, policy: &FlagPolicy) -> String {
    if trust == Trust::Blocked {
        return String::from("blocked sender");
    }
    // Same comparison as the engine, so that NaN counts as "not confident" here too.
    let confident = verdict.confidence >= policy.min_confidence;
    if !confident {
        return format!(
            "not confident ({:.2} is below the minimum {:.2}) and the Unsure flag is disabled",
            verdict.confidence, policy.min_confidence
        );
    }
    if trust == Trust::Unknown && !verdict.category.can_be_flagged() {
        return format!(
            "category {} is never flagged for an unknown sender",
            verdict.category.as_str()
        );
    }
    String::from("nothing to do (no reply needed, deadline or payment for an enabled flag)")
}

impl Outcome {
    /// The human-readable block for one file.
    fn render(&self) -> String {
        let v = &self.verdict;
        let flag = match (&self.flag, &self.no_flag_reason) {
            (Some(flag), _) => format!("{} ({})", flag.name, flag.colour),
            (None, Some(reason)) => format!("no flag: {reason}"),
            (None, None) => String::from("no flag"),
        };
        let yes_no = |b: bool| if b { "yes" } else { "no" };
        // For a rule, say which one fired; the model's reason is printed on its own line.
        let decided_by = if self.source == SOURCE_RULE {
            format!("{} ({})", self.source, v.reason)
        } else {
            self.source.to_string()
        };
        [
            format!("== {}", self.file.display()),
            format!("From:        {}", self.from),
            format!("Subject:     {}", self.subject),
            format!("Category:    {}", v.category.as_str()),
            format!("Confidence:  {:.2}", v.confidence),
            format!("Needs reply: {}", yes_no(v.needs_reply)),
            format!(
                "Deadline:    {}",
                v.deadline
                    .map_or_else(|| "none".into(), |date| date.to_string())
            ),
            format!("Payment:     {}", v.payment.as_str()),
            format!("Reason:      {}", v.reason),
            format!("Decided by:  {decided_by}"),
            format!("Flag:        {flag}"),
            String::new(),
        ]
        .join("\n")
    }

    /// One compact JSON object (a single line).
    fn to_json(&self) -> Value {
        let v = &self.verdict;
        json!({
            "file": self.file.display().to_string(),
            "from": self.from,
            "subject": self.subject,
            "category": v.category.as_str(),
            "confidence": v.confidence,
            "needs_reply": v.needs_reply,
            "deadline": v.deadline.map(|date| date.to_string()),
            "payment": v.payment.as_str(),
            "reason": v.reason,
            "source": self.source,
            "flag": self.flag.as_ref().map(|f| json!({"name": f.name, "colour": f.colour})),
            "no_flag_reason": self.no_flag_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use lets_triage_mail::engine::{Category, Payment};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// A model server that always answers with `verdict_json`.
    async fn fake_model(verdict_json: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"choices": [{"message": {"role": "assistant", "content": verdict_json}}]}),
            ))
            .mount(&server)
            .await;
        server
    }

    fn config_for(server: &MockServer) -> Config {
        Config::parse(&format!(
            "[llm]\nbase_url = \"{}/v1\"\nmodel = \"fake\"\n",
            server.uri()
        ))
        .unwrap()
    }

    fn decider(config: &Config) -> LlmDecider {
        LlmDecider::new(LlmSettings::from(&config.llm)).unwrap()
    }

    const REPLY: &str = r#"{"category":"personal","needs_reply":true,"deadline":null,"payment":"not_applicable","confidence":0.93,"reason":"A friend asks a question."}"#;

    #[tokio::test]
    async fn personal_mail_that_needs_a_reply_gets_the_reply_flag() {
        let server = fake_model(REPLY).await;
        let config = config_for(&server);
        let outcome = classify_file(&decider(&config), &config, None, &fixture("friend.eml"))
            .await
            .unwrap();

        assert_eq!(outcome.subject, "Lunch?");
        assert_eq!(outcome.from, "Anna <anna@friend.example>");
        let flag = outcome.flag.as_ref().expect("a flag");
        assert_eq!(
            (flag.name.as_str(), flag.colour.as_str()),
            ("Reply needed", "orange")
        );

        let text = outcome.render();
        assert!(text.contains("Category:    personal"), "{text}");
        assert!(
            text.contains("Flag:        Reply needed (orange)"),
            "{text}"
        );
        // The body must never be printed.
        assert!(!text.contains("free on Friday"), "{text}");
    }

    #[tokio::test]
    async fn json_output_is_one_parseable_object() {
        let server = fake_model(REPLY).await;
        let config = config_for(&server);
        let outcome = classify_file(&decider(&config), &config, None, &fixture("friend.eml"))
            .await
            .unwrap();

        let line = outcome.to_json().to_string();
        assert!(!line.contains('\n'));
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["category"], "personal");
        assert_eq!(value["needs_reply"], true);
        assert_eq!(value["flag"]["colour"], "orange");
        assert_eq!(value["no_flag_reason"], Value::Null);
        assert!(!line.contains("free on Friday"));
    }

    #[tokio::test]
    async fn the_model_is_named_as_the_source_of_its_verdict() {
        let server = fake_model(REPLY).await;
        let config = config_for(&server);
        let outcome = classify_file(&decider(&config), &config, None, &fixture("friend.eml"))
            .await
            .unwrap();
        assert!(outcome.render().contains("Decided by:  model"));
        assert_eq!(outcome.to_json()["source"], "model");
    }

    #[tokio::test]
    async fn a_rule_decides_certain_spam_and_the_model_is_not_called() {
        // The fake model would answer "personal, reply needed"; any request would show.
        let server = fake_model(REPLY).await;
        let config = config_for(&server);
        let outcome = classify_file(
            &decider(&config),
            &config,
            None,
            &fixture("casino_rotating.eml"),
        )
        .await
        .unwrap();

        assert!(server.received_requests().await.unwrap().is_empty());
        assert_eq!(outcome.verdict.category, Category::Spam);
        assert!(outcome.flag.is_none());
        let text = outcome.render();
        assert!(
            text.contains("Decided by:  rule (invalid sender address)"),
            "{text}"
        );
        let json = outcome.to_json();
        assert_eq!(json["source"], "rule");
        assert_eq!(json["category"], "spam");
    }

    #[tokio::test]
    async fn provider_headers_reach_the_parser_only_when_configured() {
        // Agency mail is not decided by a rule either way: the model is asked.
        let server = fake_model(REPLY).await;
        let config = config_for(&server);
        let wp = Some(ProviderHeaders::Wp);
        let outcome = classify_file(
            &decider(&config),
            &config,
            wp,
            &fixture("agency_marketing.eml"),
        )
        .await
        .unwrap();
        assert_eq!(outcome.source, "model");
        // The prompt carries the provider facts, so the model saw them.
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body).to_string();
        assert!(
            body.contains("authentication: spf=unknown dkim=pass"),
            "{body}"
        );
        assert!(body.contains("(a hint only): spam"), "{body}");
    }

    #[tokio::test]
    async fn newsletters_get_no_flag_and_say_why() {
        let verdict = r#"{"category":"newsletter","needs_reply":false,"deadline":null,"payment":"not_applicable","confidence":0.9,"reason":"A digest."}"#;
        let server = fake_model(verdict).await;
        let config = config_for(&server);
        let outcome = classify_file(&decider(&config), &config, None, &fixture("newsletter.eml"))
            .await
            .unwrap();
        assert!(outcome.flag.is_none());
        let reason = outcome.no_flag_reason.as_deref().unwrap();
        assert!(reason.contains("newsletter"), "{reason}");
    }

    #[tokio::test]
    async fn low_confidence_gets_the_unsure_flag() {
        let verdict = r#"{"category":"personal","needs_reply":true,"deadline":null,"payment":"not_applicable","confidence":0.4,"reason":"Unclear."}"#;
        let server = fake_model(verdict).await;
        let config = config_for(&server);
        let outcome = classify_file(&decider(&config), &config, None, &fixture("friend.eml"))
            .await
            .unwrap();
        assert_eq!(outcome.flag.unwrap().name, "Unsure");
    }

    #[test]
    fn explanations_cover_each_reason() {
        let policy = FlagPolicy::default();
        let mut verdict = Verdict {
            category: Category::Personal,
            needs_reply: false,
            deadline: None,
            payment: Payment::NotApplicable,
            confidence: 0.9,
            reason: String::new(),
        };
        assert!(explain_no_flag(Trust::Blocked, &verdict, &policy).contains("blocked"));
        assert!(explain_no_flag(Trust::Unknown, &verdict, &policy).contains("nothing to do"));
        verdict.category = Category::Spam;
        assert!(explain_no_flag(Trust::Unknown, &verdict, &policy).contains("spam"));
        verdict.confidence = 0.1;
        let policy = FlagPolicy {
            unsure: false,
            ..policy
        };
        assert!(explain_no_flag(Trust::Unknown, &verdict, &policy).contains("not confident"));
    }

    #[tokio::test]
    async fn a_missing_file_is_an_error_and_the_next_file_still_runs() {
        let server = fake_model(REPLY).await;
        let config = config_for(&server);
        let decider = decider(&config);

        let missing = classify_file(&decider, &config, None, Path::new("/nonexistent/x.eml")).await;
        assert!(format!("{:#}", missing.unwrap_err()).contains("reading /nonexistent/x.eml"));
        assert!(
            classify_file(&decider, &config, None, &fixture("friend.eml"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_fails_overall_when_any_file_fails() {
        let server = fake_model(REPLY).await;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!("[llm]\nbase_url = \"{}/v1\"\n", server.uri()),
        )
        .unwrap();

        let files = [fixture("friend.eml"), PathBuf::from("/nonexistent/x.eml")];
        let error = run(&config_path, &files, true, None).await.unwrap_err();
        assert!(error.to_string().contains("1 of 2"), "{error}");
        assert!(run(&config_path, &files[..1], false, None).await.is_ok());
    }

    #[test]
    fn provider_headers_come_from_the_named_account() {
        let config = Config::parse(
            "[[accounts]]\nname = \"wp\"\nhost = \"imap.wp.pl\"\nusername = \"me@wp.pl\"\n\
             provider_headers = \"wp\"\n",
        )
        .unwrap();
        assert_eq!(provider_for(&config, None).unwrap(), None);
        assert_eq!(
            provider_for(&config, Some("wp")).unwrap(),
            Some(ProviderHeaders::Wp)
        );
        let error = provider_for(&config, Some("gmail")).unwrap_err();
        assert!(error.to_string().contains("accounts: wp"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_config_points_at_the_example() {
        let missing = Path::new("/nonexistent/config.toml");
        let error = run(missing, &[fixture("friend.eml")], false, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("config.example.toml"), "{error}");
    }

    #[tokio::test]
    async fn an_unreachable_model_is_a_clean_error() {
        let config = Config::parse("[llm]\nbase_url = \"http://127.0.0.1:1/v1\"\n").unwrap();
        let error = classify_file(&decider(&config), &config, None, &fixture("friend.eml"))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("could not reach the model"),
            "{error}"
        );
    }
}
