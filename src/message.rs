//! Turn raw RFC 822 bytes into the facts the classifier needs.
//!
//! Email content is hostile input: this module only *reads* it and never acts on it. It does no
//! IO (the caller hands in bytes), so every behaviour here is unit-testable with `.eml` fixtures.
//!
//! # Ownership
//!
//! `mail-parser` is zero-copy: its `Message<'x>` borrows from the input buffer. We copy what we
//! need into owned `String`s, so a [`ParsedMessage`] has no lifetime and can outlive the buffer
//! and move between tasks. The price is a few small allocations per message, which is nothing
//! next to a model call.

use std::fmt;

use mail_parser::{HeaderName, Headers, MessageParser};
use scraper::{Html, Node};
use serde::Deserialize;
use thiserror::Error;

/// Messages larger than this should be parsed with [`parse_headers_only`]: decoding and
/// flattening a huge body costs memory and the classifier truncates it anyway.
pub const HEADERS_ONLY_THRESHOLD: usize = 2 * 1024 * 1024;

/// At most this many attachment names are kept; the names are a signal, not an inventory.
const MAX_ATTACHMENT_NAMES: usize = 10;

/// Why a message could not be parsed at all.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    /// The bytes have no header block, so they are not an RFC 822 message.
    #[error("input is not a parseable RFC 822 message")]
    Unparseable,
}

/// The outcome of one authentication check (SPF, DKIM or DMARC) from `Authentication-Results`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthVerdict {
    /// The check passed.
    Pass,
    /// The check failed.
    Fail,
    /// SPF "softfail": the sender is probably not authorised, but the domain only asks for suspicion.
    SoftFail,
    /// The domain made no assertion.
    Neutral,
    /// The check was not run or the header is missing. This is the default.
    #[default]
    None,
    /// A temporary or permanent error while checking (`temperror`, `permerror`).
    Error,
    /// A result keyword we do not know.
    Other,
}

impl AuthVerdict {
    /// Map the keyword after `=` in `Authentication-Results` (case-insensitive).
    fn from_keyword(word: &str) -> Self {
        match word.to_ascii_lowercase().as_str() {
            "pass" => AuthVerdict::Pass,
            "fail" => AuthVerdict::Fail,
            "softfail" => AuthVerdict::SoftFail,
            "neutral" => AuthVerdict::Neutral,
            "none" => AuthVerdict::None,
            "temperror" | "permerror" => AuthVerdict::Error,
            _ => AuthVerdict::Other,
        }
    }

    /// The lowercase keyword, for prompts and logs.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthVerdict::Pass => "pass",
            AuthVerdict::Fail => "fail",
            AuthVerdict::SoftFail => "softfail",
            AuthVerdict::Neutral => "neutral",
            AuthVerdict::None => "none",
            AuthVerdict::Error => "error",
            AuthVerdict::Other => "other",
        }
    }
}

/// SPF, DKIM and DMARC results taken from one trusted `Authentication-Results` header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthResults {
    /// SPF result.
    pub spf: AuthVerdict,
    /// DKIM result; with several signatures, one pass is enough.
    pub dkim: AuthVerdict,
    /// DMARC result.
    pub dmarc: AuthVerdict,
    /// Domains (lowercase) whose DKIM signature passed.
    pub dkim_domains: Vec<String>,
}

impl AuthResults {
    /// True when any of the three checks outright failed. Softfail does not count.
    pub fn any_fail(&self) -> bool {
        [self.spf, self.dkim, self.dmarc].contains(&AuthVerdict::Fail)
    }

    /// All three checks passed.
    pub fn all_pass(&self) -> bool {
        [self.spf, self.dkim, self.dmarc]
            .iter()
            .all(|v| *v == AuthVerdict::Pass)
    }

    /// A short line such as `spf=pass dkim=pass dmarc=none`, for prompts and logs.
    pub fn summary(&self) -> String {
        format!(
            "spf={} dkim={} dmarc={}",
            self.spf.as_str(),
            self.dkim.as_str(),
            self.dmarc.as_str()
        )
    }
}

/// A mail provider whose own spam and DKIM headers we know how to read.
///
/// This is opt-in (`provider_headers` on an `[[accounts]]` entry) because a *sender* can write
/// any header they like, including a fake `X-WP-DKIM-Status: good`. Only a provider that
/// strips or overwrites these headers on arrival makes them trustworthy, and only the user
/// knows whether theirs does. To support another provider, add a variant here and a match arm
/// in [`apply_provider_headers`]; there is deliberately no plugin system.
///
/// `Deserialize` lets `serde` parse the config value. `rename_all` makes the TOML spelling
/// lowercase (`"wp"`), and a typo gets an error that lists the known values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderHeaders {
    /// wp.pl: `X-WP-DKIM-Status` and `X-WP-SPAM`.
    Wp,
}

/// Everything the classifier needs to know about one message.
///
/// **Privacy:** `body_text` holds the flattened message body because the decider needs it for
/// the prompt. It lives in memory only for the duration of one classification. Never write it
/// to the database or to logs (see `CLAUDE.md`: metadata only). `Debug` is written by hand
/// so that `{:?}` shows only the body's length, never its text.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct ParsedMessage {
    /// `Message-ID` without angle brackets.
    pub message_id: Option<String>,
    /// `In-Reply-To` ids without angle brackets.
    pub in_reply_to: Vec<String>,
    /// `References` ids without angle brackets.
    pub references: Vec<String>,
    /// Display name of the sender (may be empty). Untrusted: anyone can write anything here.
    pub from_name: String,
    /// Sender address, lowercase (may be empty for broken messages).
    pub from_address: String,
    /// First `Reply-To` address, lowercase.
    pub reply_to: Option<String>,
    /// `To` addresses, lowercase.
    pub to: Vec<String>,
    /// `Cc` addresses, lowercase.
    pub cc: Vec<String>,
    /// Decoded subject.
    pub subject: String,
    /// `Date` as RFC 3339, when it parses.
    pub date: Option<String>,
    /// Flattened plain-text body. **Must not be persisted or logged.** Empty in headers-only mode.
    pub body_text: String,
    /// Number of links in the message (HTML anchors, or URLs in a plain-text body).
    pub link_count: usize,
    /// Attachment file names, at most ten (`"(unnamed)"` when a part has none).
    pub attachment_names: Vec<String>,
    /// True when the message has at least one attachment.
    pub has_attachments: bool,
    /// Authentication results from the trusted `Authentication-Results` header.
    pub auth: AuthResults,
    /// The provider's own spam verdict (`Some(true)` = "spam"), read only when the user opted
    /// in to [`ProviderHeaders`]. Evidence for the model, never a verdict by itself: providers
    /// also mark legitimate bulk mail as spam.
    pub provider_spam: Option<bool>,
    /// A `List-Unsubscribe` header is present. We never act on it; it is a bulk-mail signal.
    pub has_list_unsubscribe: bool,
    /// `List-Id` header, trimmed.
    pub list_id: Option<String>,
    /// `Precedence` header, trimmed.
    pub precedence: Option<String>,
    /// `Auto-Submitted` is present and not `no` (RFC 3834): a machine sent this.
    pub is_auto_submitted: bool,
    /// Size in bytes of the input that was parsed. In headers-only mode that is just the headers
    /// the caller fetched, so a caller that knows the real size (IMAP `RFC822.SIZE`) should
    /// overwrite it.
    pub size: usize,
    /// True when only headers were parsed and the body-derived fields are empty.
    pub headers_only: bool,
}

impl fmt::Debug for ParsedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedMessage")
            .field("message_id", &self.message_id)
            .field("in_reply_to", &self.in_reply_to)
            .field("references", &self.references)
            .field("from_name", &self.from_name)
            .field("from_address", &self.from_address)
            .field("reply_to", &self.reply_to)
            .field("to", &self.to)
            .field("cc", &self.cc)
            .field("subject", &self.subject)
            .field("date", &self.date)
            .field(
                "body_text",
                &format_args!("<{} chars, hidden>", self.body_text.chars().count()),
            )
            .field("link_count", &self.link_count)
            .field("attachment_names", &self.attachment_names)
            .field("has_attachments", &self.has_attachments)
            .field("auth", &self.auth)
            .field("provider_spam", &self.provider_spam)
            .field("has_list_unsubscribe", &self.has_list_unsubscribe)
            .field("list_id", &self.list_id)
            .field("precedence", &self.precedence)
            .field("is_auto_submitted", &self.is_auto_submitted)
            .field("size", &self.size)
            .field("headers_only", &self.headers_only)
            .finish()
    }
}

impl ParsedMessage {
    /// Domain part of the sender address (empty when there is none).
    pub fn from_domain(&self) -> &str {
        domain_of(&self.from_address)
    }

    /// Looks like bulk or list mail: unsubscribe header, `List-Id`, or `Precedence: bulk/list/junk`.
    pub fn is_bulk(&self) -> bool {
        self.has_list_unsubscribe
            || self.list_id.is_some()
            || self.precedence.as_deref().is_some_and(|p| {
                ["bulk", "list", "junk"].contains(&p.to_ascii_lowercase().as_str())
            })
    }

    /// The display name contains an address that differs from the real sender,
    /// as in `"support@paypal.com" <x@evil.example>`.
    pub fn display_name_spoof(&self) -> bool {
        emails_in_text(&self.from_name)
            .iter()
            .any(|found| *found != self.from_address)
    }

    /// The `From` address is not a valid address (see [`is_malformed_address`]).
    ///
    /// `mail-parser` hands such an address back unchanged, so we can judge it. A message with
    /// no `From` address at all (`from_address` empty) is *not* reported here: that is
    /// "unknown", not "invalid", and calling it spam would be too harsh for a broken but
    /// real message.
    pub fn malformed_sender(&self) -> bool {
        !self.from_address.is_empty() && is_malformed_address(&self.from_address)
    }

    /// A stranger writes "as" the recipient; [`impersonates_recipient`] holds the rules and
    /// their trade-offs. The recipients are this message's `To` and `Cc` addresses.
    pub fn impersonates_recipient(&self) -> bool {
        let recipients: Vec<&str> = self.to.iter().chain(&self.cc).map(String::as_str).collect();
        impersonates_recipient(&self.from_name, &self.from_address, &recipients)
    }

    /// `Reply-To` points at a different domain than `From`: replies would go somewhere else.
    pub fn reply_to_mismatch(&self) -> bool {
        match &self.reply_to {
            Some(reply_to) if reply_to.contains('@') => domain_of(reply_to) != self.from_domain(),
            _ => false,
        }
    }
}

/// Parse a full message: headers, body, attachments.
///
/// `provider_headers` opts in to reading one provider's own DKIM and spam headers (see
/// [`ProviderHeaders`]); `None` ignores them. A standard `Authentication-Results` header,
/// when present, still wins for the methods it reports.
///
/// `authserv_id` is the hostname our own mail server stamps on `Authentication-Results`. When
/// `Some`, only headers from that server are trusted, so a sender cannot inject a forged
/// `dkim=pass` header. When `None`, the topmost header is used (the one added last, by the
/// receiving server).
pub fn parse_message(
    raw: &[u8],
    authserv_id: Option<&str>,
    provider_headers: Option<ProviderHeaders>,
) -> Result<ParsedMessage, ParseError> {
    parse(raw, authserv_id, provider_headers, false)
}

/// Parse headers only; body, links and attachments stay empty. Use this for messages over
/// [`HEADERS_ONLY_THRESHOLD`].
pub fn parse_headers_only(
    raw: &[u8],
    authserv_id: Option<&str>,
    provider_headers: Option<ProviderHeaders>,
) -> Result<ParsedMessage, ParseError> {
    parse(raw, authserv_id, provider_headers, true)
}

fn parse(
    raw: &[u8],
    authserv_id: Option<&str>,
    provider_headers: Option<ProviderHeaders>,
    headers_only: bool,
) -> Result<ParsedMessage, ParseError> {
    let parser = MessageParser::default();
    let message = if headers_only {
        parser.parse_headers(raw)
    } else {
        parser.parse(raw)
    }
    .ok_or(ParseError::Unparseable)?;

    let from = message.from().and_then(|list| list.first());
    let headers = message.headers();

    // Option chaining: each `and_then` / `map` runs only if the previous step produced a value.
    let reply_to = message.reply_to().and_then(|list| {
        list.mailboxes()
            .find_map(|m| m.address().map(str::to_lowercase))
    });

    let mut parsed = ParsedMessage {
        message_id: message.message_id().map(str::to_owned),
        in_reply_to: message
            .in_reply_to()
            .map(|ids| ids.iter().map(str::to_owned).collect())
            .unwrap_or_default(),
        references: message
            .references()
            .map(|ids| ids.iter().map(str::to_owned).collect())
            .unwrap_or_default(),
        from_name: from
            .and_then(|m| m.name())
            .unwrap_or_default()
            .trim()
            .to_owned(),
        from_address: from
            .and_then(|m| m.address())
            .unwrap_or_default()
            .trim()
            .to_lowercase(),
        reply_to,
        to: message
            .to()
            .map(|list| lowercase_addresses(list.mailboxes()))
            .unwrap_or_default(),
        cc: message
            .cc()
            .map(|list| lowercase_addresses(list.mailboxes()))
            .unwrap_or_default(),
        subject: message.subject().unwrap_or_default().trim().to_owned(),
        date: message.date().map(|d| d.to_rfc3339()),
        auth: parse_auth_results(&auth_headers(&headers), authserv_id),
        has_list_unsubscribe: headers.contains(HeaderName::ListUnsubscribe),
        list_id: header_text(&headers, "List-Id"),
        precedence: header_text(&headers, "Precedence"),
        is_auto_submitted: header_text(&headers, "Auto-Submitted")
            .is_some_and(|v| !v.eq_ignore_ascii_case("no")),
        size: raw.len(),
        headers_only,
        ..ParsedMessage::default()
    };

    if let Some(provider) = provider_headers {
        apply_provider_headers(provider, &headers, &mut parsed);
    }
    if !headers_only {
        fill_body(&message, &mut parsed);
    }
    Ok(parsed)
}

fn lowercase_addresses<'m>(
    mailboxes: impl Iterator<Item = mail_parser::Mailbox<'m>>,
) -> Vec<String> {
    mailboxes
        .filter_map(|m| m.address())
        .map(str::to_lowercase)
        .collect()
}

/// Collect body text, link count and attachments from the MIME tree.
fn fill_body(message: &mail_parser::Message<'_>, parsed: &mut ParsedMessage) {
    let mut plain: Option<String> = None;
    let mut html: Option<String> = None;
    let mut attachments = Vec::new();

    for part in message.parts() {
        if part.is_multipart() {
            continue;
        }
        if part.is_attachment() {
            attachments.push(part.attachment_name().unwrap_or("(unnamed)").to_owned());
            continue;
        }
        // Keep the first text/plain and the first text/html; later ones are usually
        // quoted copies or signatures.
        // A message without a Content-Type header is plain text by RFC 2045 default, which is
        // why we test "html or not" instead of asking for `text/plain` explicitly.
        if part.is_content_type("text", "html") {
            if html.is_none() {
                html = part.text().map(|t| t.into_owned());
            }
        } else if part.is_text() && plain.is_none() {
            plain = part.text().map(|t| t.into_owned());
        }
    }

    let (html_text, html_links) = match &html {
        Some(source) => {
            let (text, links) = html_to_text(source);
            (Some(text), links)
        }
        None => (None, 0),
    };

    // Prefer the plain-text alternative when it has content: it is what the sender wrote for
    // humans, and it cannot hide text with CSS the way HTML can.
    let (text, link_count) = match (plain, html_text) {
        (Some(p), _) if !p.trim().is_empty() => {
            let links = if html_links > 0 {
                html_links
            } else {
                count_urls(&p)
            };
            (p, links)
        }
        (_, Some(h)) => (h, html_links),
        _ => (String::new(), 0),
    };

    parsed.body_text = normalize_text(&text);
    parsed.link_count = link_count;
    parsed.has_attachments = !attachments.is_empty();
    attachments.truncate(MAX_ATTACHMENT_NAMES);
    parsed.attachment_names = attachments;
}

/// Raw text of the first header with this name: unfolded, lossily decoded, trimmed.
/// (`Headers::get` returns the *last* header, which is wrong for "first wins" semantics.)
fn header_text(headers: &Headers<'_>, name: &str) -> Option<String> {
    headers
        .all(name)
        .next()
        .map(|h| unfold(h.raw_value()))
        .filter(|v| !v.is_empty())
}

/// Every `Authentication-Results` header in document order, so topmost first.
fn auth_headers(headers: &Headers<'_>) -> Vec<String> {
    headers
        .all("Authentication-Results")
        .map(|h| unfold(h.raw_value()))
        .collect()
}

/// Join folded header lines into one line with single spaces.
fn unfold(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Read SPF/DKIM/DMARC from `Authentication-Results` header values (topmost first).
///
/// Which header to trust: if `authserv_id` is given, the first header whose authserv-id matches
/// (case-insensitive); otherwise the topmost one. Headers further down may have been written by
/// the sender, so they are never mixed in.
pub fn parse_auth_results(headers: &[String], authserv_id: Option<&str>) -> AuthResults {
    let chosen = headers.iter().find(|value| match authserv_id {
        Some(wanted) => value
            .split(';')
            .next()
            .and_then(|head| head.split_whitespace().next())
            .is_some_and(|id| id.eq_ignore_ascii_case(wanted.trim())),
        None => true,
    });

    let mut results = AuthResults::default();
    let Some(chosen) = chosen else {
        return results;
    };

    // Format: `authserv-id; method=result props...; method=result props...`.
    // The first clause is the server id, so skip it.
    for clause in chosen.split(';').skip(1) {
        let clause = clause.trim();
        let Some((method, rest)) = clause.split_once('=') else {
            continue;
        };
        let method = method.trim().to_ascii_lowercase();
        let verdict = AuthVerdict::from_keyword(
            rest.trim_start()
                .split(|c: char| !c.is_ascii_alphabetic())
                .next()
                .unwrap_or_default(),
        );
        match method.as_str() {
            "dkim" => {
                if verdict == AuthVerdict::Pass {
                    // One passing signature is enough, even if another signature failed.
                    results.dkim = AuthVerdict::Pass;
                    if let Some(domain) = dkim_domain(clause) {
                        results.dkim_domains.push(domain);
                    }
                } else if results.dkim == AuthVerdict::None {
                    results.dkim = verdict;
                }
            }
            // First result wins for the other methods.
            "spf" if results.spf == AuthVerdict::None => results.spf = verdict,
            "dmarc" if results.dmarc == AuthVerdict::None => results.dmarc = verdict,
            _ => {}
        }
    }
    results
}

/// The signing domain from `header.d=example.com` (or `header.i=@example.com`).
fn dkim_domain(clause: &str) -> Option<String> {
    clause.split_whitespace().find_map(|token| {
        let value = token
            .strip_prefix("header.d=")
            .or_else(|| token.strip_prefix("header.i="))?;
        let domain = value.trim_matches('"');
        let domain = domain.rsplit('@').next().unwrap_or(domain);
        (!domain.is_empty()).then(|| domain.to_lowercase())
    })
}

/// Fills in what a provider's own headers say, for the methods `Authentication-Results` did
/// not cover. The standard header wins: if it already gave a DKIM result, the provider's is
/// ignored.
fn apply_provider_headers(
    provider: ProviderHeaders,
    headers: &Headers<'_>,
    out: &mut ParsedMessage,
) {
    match provider {
        ProviderHeaders::Wp => {
            if out.auth.dkim == AuthVerdict::None {
                if let Some(value) = header_text(headers, "X-WP-DKIM-Status") {
                    out.auth.dkim = parse_wp_dkim(&value);
                }
            }
            out.provider_spam = header_text(headers, "X-WP-SPAM").and_then(|v| parse_wp_spam(&v));
        }
    }
}

/// `X-WP-DKIM-Status` value to a DKIM result: `good (id: example.com)` is a pass, `bad` or
/// `invalid` a fail, `no signature` (and anything else) means "no result".
fn parse_wp_dkim(value: &str) -> AuthVerdict {
    let first = value.split_whitespace().next().unwrap_or_default();
    match first.to_ascii_lowercase().as_str() {
        "good" => AuthVerdict::Pass,
        "bad" | "invalid" => AuthVerdict::Fail,
        _ => AuthVerdict::None,
    }
}

/// `X-WP-SPAM: YES (U9) 0000019 [IAYE]` or `NO ...` to the provider's verdict.
fn parse_wp_spam(value: &str) -> Option<bool> {
    let first = value.split_whitespace().next()?;
    match first.to_ascii_uppercase().as_str() {
        "YES" => Some(true),
        "NO" => Some(false),
        _ => None,
    }
}

/// True when `address` (`local@domain`) cannot be a real mailbox.
///
/// Checked: an empty local part or domain, an empty domain label (this covers `..` anywhere
/// and a leading or trailing dot), a domain that starts or ends with `-`, a domain without a
/// dot, and characters a domain may not contain. Non-ASCII letters are allowed, because
/// internationalised domains exist. It is deliberately not a full RFC 5322 validator: it
/// catches the generated junk seen in rotating-address spam, and a real mailbox passes.
///
/// A domain without a dot (`user@localhost`) is flagged although it is technically valid:
/// nobody legitimate mails us from a single-label domain over the internet.
pub fn is_malformed_address(address: &str) -> bool {
    let Some((local, domain)) = address.rsplit_once('@') else {
        return true;
    };
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return true;
    }
    if domain.starts_with('-') || domain.ends_with('-') {
        return true;
    }
    // `split('.')` yields an empty label for `..`, a leading dot and a trailing dot.
    domain.split('.').any(str::is_empty)
        || !domain
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '.')
}

/// Shortest recipient local part for which a look-alike is meaningful. `anna` or `piotr` is
/// shared by thousands of real people, so mail that merely starts with it proves nothing.
const MIN_IMPERSONATED_LOCAL_LEN: usize = 6;

/// A stranger's `From` uses the recipient's own identity.
///
/// Spam generators put the victim's name into the sender to look personal. `recipients` are
/// the message's `To` and `Cc` addresses (lowercase). It never fires when the `From` address
/// is one of the recipients (mail to yourself, a mailing list that sends to its own address).
/// Otherwise it fires when, for some recipient:
///
/// 1. the display name is exactly that recipient's address; or
/// 2. the display name is exactly the recipient's local part; or
/// 3. the `From` local part is the recipient's local part, then one of `.` `_` `-` `+`, then
///    a random-looking tail (see [`looks_random`]), and the `From` domain differs from the
///    recipient's domain.
///
/// # Trade-offs (false positives versus misses)
///
/// Rules 2 and 3 only apply to a *distinctive* recipient local part: at least
/// [`MIN_IMPERSONATED_LOCAL_LEN`] characters and containing a separator or a digit
/// (`me.name`, `piotr77`; not `piotr`). That keeps `piotr.kowalski@firm.example` writing to
/// `piotr@home.example` from firing. The price: someone whose address is a plain first name
/// is not protected by rules 2 and 3 (rule 1 still works). Rule 3 can still hit a real sender
/// who appends digits to your address, such as `me.name.2024@other.example`; the caller
/// therefore acts on the result only together with a missing DKIM pass.
pub fn impersonates_recipient(from_name: &str, from_address: &str, recipients: &[&str]) -> bool {
    if from_address.is_empty() || recipients.contains(&from_address) {
        return false;
    }
    let name = from_name.trim().to_lowercase();
    let from_local = local_of(from_address);
    let from_domain = domain_of(from_address);

    recipients.iter().any(|&recipient| {
        // Rule 1: the display name is the victim's own address. Specific enough on its own.
        if !name.is_empty() && name == recipient {
            return true;
        }
        let local = local_of(recipient);
        if !is_distinctive_local(local) {
            return false;
        }
        // Rule 2.
        if name == local {
            return true;
        }
        // Rule 3: `<local><separator><tail>` from another domain.
        let tail = from_local
            .strip_prefix(local)
            .and_then(|rest| rest.strip_prefix(['.', '_', '-', '+']));
        tail.is_some_and(looks_random) && from_domain != domain_of(recipient)
    })
}

/// Long enough, and not a bare first name: it has a separator or a digit.
fn is_distinctive_local(local: &str) -> bool {
    local.chars().count() >= MIN_IMPERSONATED_LOCAL_LEN
        && local
            .chars()
            .any(|c| c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// Does this address tail look machine-generated, like `2lkxp6bb.fnvrb`?
///
/// Ignoring the separators it must be ASCII letters and digits only, at least five
/// characters, and either contain a digit or contain a run of four or more letters without a
/// vowel (`fskpfqvf`). Words such as `kowalski`, `work` or `newsletter` have vowels and no
/// digits, so they pass as human-chosen. `y` counts as a vowel, which is right for Polish
/// and English words and rarely matters for random strings.
fn looks_random(tail: &str) -> bool {
    let is_separator = |c: char| matches!(c, '.' | '_' | '-' | '+');
    let compact: String = tail.chars().filter(|&c| !is_separator(c)).collect();
    if compact.len() < 5 || !compact.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    compact.chars().any(|c| c.is_ascii_digit())
        || tail.split(is_separator).any(|segment| {
            segment.len() >= 4
                && !segment
                    .chars()
                    .any(|c| "aeiouy".contains(c.to_ascii_lowercase()))
        })
}

fn local_of(address: &str) -> &str {
    address.rsplit_once('@').map_or(address, |(local, _)| local)
}

fn domain_of(address: &str) -> &str {
    address.rsplit_once('@').map_or("", |(_, domain)| domain)
}

/// Find things that look like email addresses in free text, lowercased.
fn emails_in_text(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "<>\"'(),;:[]".contains(c))
        .filter_map(|token| {
            let token = token.trim_matches('.');
            let (local, domain) = token.split_once('@')?;
            let plausible = !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.contains('@');
            plausible.then(|| token.to_lowercase())
        })
        .collect()
}

fn count_urls(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| word.starts_with("http://") || word.starts_with("https://"))
        .count()
}

/// Tags whose content is not part of the visible message.
const SKIPPED_TAGS: [&str; 6] = ["script", "style", "head", "title", "noscript", "template"];

/// Tags that start a new line in the flattened text.
const BLOCK_TAGS: [&str; 15] = [
    "p",
    "div",
    "br",
    "li",
    "tr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "table",
    "section",
    "ul",
    "blockquote",
];

/// HTML beyond this many bytes is ignored; the classifier only reads the start of a body anyway.
const MAX_HTML_BYTES: usize = 1024 * 1024;

/// Nesting deeper than this is treated as an attack, not an email.
const MAX_HTML_DEPTH: usize = 1000;

/// Tags that never nest, or that HTML closes implicitly, so they do not add depth.
const FLAT_TAGS: [&str; 18] = [
    "br", "hr", "img", "meta", "link", "input", "p", "li", "tr", "td", "th", "dt", "dd", "option",
    "col", "area", "base", "wbr",
];

/// Cut a string to at most `max` bytes without splitting a UTF-8 character.
fn truncate_at_char_boundary(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// A cheap pre-scan that estimates the maximum tag nesting without building a tree.
/// It only needs to be roughly right: it guards against pathological input, nothing more.
fn too_deeply_nested(source: &str) -> bool {
    let mut depth = 0usize;
    let mut rest = truncate_at_char_boundary(source, MAX_HTML_BYTES);
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let closing = rest.starts_with('/');
        let tag = rest.trim_start_matches('/');
        let name_len = tag
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tag.len());
        if name_len == 0 {
            continue; // `<` followed by something that is not a tag name
        }
        let name = tag[..name_len].to_ascii_lowercase();
        if FLAT_TAGS.contains(&name.as_str()) {
            continue;
        }
        if closing {
            depth = depth.saturating_sub(1);
        } else {
            depth += 1;
            if depth > MAX_HTML_DEPTH {
                return true;
            }
        }
    }
    false
}

/// Flatten HTML to text and count its links.
///
/// `scraper` (built on html5ever, the HTML5 parsing algorithm) accepts any input without
/// panicking: broken markup is repaired the way a browser would. Scripts, styles and the
/// `<head>` are dropped so their content never reaches the model.
pub fn html_to_text(source: &str) -> (String, usize) {
    use ego_tree::iter::Edge;

    // html5ever slows down quadratically on deeply nested markup (16,000 nested `<div>`s took
    // seconds), so a hostile sender could stall the service. Real mail never nests that deep.
    // Refusing such input costs us only that message's body text.
    if too_deeply_nested(source) {
        return (String::new(), 0);
    }
    let source = truncate_at_char_boundary(source, MAX_HTML_BYTES);

    let document = Html::parse_document(source);
    let mut text = String::new();
    let mut links = 0;
    // How many skipped elements we are currently inside. An iterative walk (`traverse`) instead
    // of recursion, so absurdly deep nesting cannot overflow the stack.
    let mut skip_depth = 0usize;

    for edge in document.tree.root().traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Element(el) => {
                    let name = el.name();
                    if SKIPPED_TAGS.contains(&name) {
                        skip_depth += 1;
                    } else if skip_depth == 0 {
                        if BLOCK_TAGS.contains(&name) {
                            text.push('\n');
                        }
                        if name == "a" && el.attr("href").is_some() {
                            links += 1;
                        }
                    }
                }
                Node::Text(t) if skip_depth == 0 => text.push_str(&t.text),
                _ => {}
            },
            Edge::Close(node) => {
                if let Node::Element(el) = node.value() {
                    let name = el.name();
                    if SKIPPED_TAGS.contains(&name) {
                        skip_depth = skip_depth.saturating_sub(1);
                    } else if skip_depth == 0 && BLOCK_TAGS.contains(&name) {
                        text.push('\n');
                    }
                }
            }
        }
    }
    (text, links)
}

/// Tidy extracted text: drop zero-width characters, turn non-breaking spaces into spaces,
/// collapse runs of whitespace and limit blank lines to one.
fn normalize_text(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}'))
        .map(|c| if c == '\u{a0}' { ' ' } else { c })
        .collect();

    let mut out = String::new();
    let mut blank_run = 0;
    for line in cleaned.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_addresses_pass() {
        for ok in [
            "anna@friend.example",
            "a.b+tag@mail.example.co.uk",
            "x@sub-domain.example",
            "jan@zażółć.example",
        ] {
            assert!(!is_malformed_address(ok), "{ok}");
        }
    }

    #[test]
    fn malformed_addresses_are_caught() {
        for bad in [
            "me.name.2lkxp6bb.fnvrb@volny.example..sn.live.example", // `..` in the domain
            "@example.com",                                          // empty local part
            "anna@",                                                 // empty domain
            "anna@.example.com",                                     // leading dot
            "anna@example.com.",                                     // trailing dot
            "anna@-example.com",                                     // leading hyphen
            "anna@example.com-",                                     // trailing hyphen
            "anna@localhost",                                        // no dot
            "anna@exa mple.com",                                     // space
            "anna@exam_ple.com",                                     // underscore
            "anna@exam!ple.com",                                     // punctuation
            "not-an-address",                                        // no @
        ] {
            assert!(is_malformed_address(bad), "{bad}");
        }
    }

    #[test]
    fn a_missing_sender_is_not_called_malformed() {
        assert!(!ParsedMessage::default().malformed_sender());
    }

    const ME: &str = "me.name@provider.example";

    #[test]
    fn rotating_address_spam_impersonates_the_recipient() {
        // Local part starts with ours, then a random tail, on another domain.
        assert!(impersonates_recipient(
            "Casino",
            "me.name.2lkxp6bb.fnvrb@other.example",
            &[ME]
        ));
        // `_` as the separator, no digit, but a vowel-less run.
        assert!(impersonates_recipient(
            "x",
            "me.name_fskpfqvf@other.example",
            &[ME]
        ));
        // The display name is our local part, or our whole address.
        assert!(impersonates_recipient(
            "me.name",
            "zzz@other.example",
            &[ME]
        ));
        assert!(impersonates_recipient(
            "Me.Name",
            "zzz@other.example",
            &[ME]
        ));
        assert!(impersonates_recipient(ME, "zzz@other.example", &[ME]));
        // A Cc recipient counts too.
        assert!(impersonates_recipient(
            "me.name",
            "zzz@other.example",
            &["someone@else.example", ME]
        ));
    }

    #[test]
    fn real_people_and_lists_do_not_impersonate() {
        // A colleague with the same first name: our local part is too short and plain.
        assert!(!impersonates_recipient(
            "Piotr Kowalski",
            "piotr.kowalski@firm.example",
            &["piotr@home.example"]
        ));
        // Even a distinctive local part is safe against a human-looking tail.
        assert!(!impersonates_recipient(
            "x",
            "me.name.work@other.example",
            &[ME]
        ));
        assert!(!impersonates_recipient(
            "x",
            "me.name.kowalski@other.example",
            &[ME]
        ));
        // Mail to ourselves, and a list that writes to its own address.
        assert!(!impersonates_recipient("me.name", ME, &[ME]));
        assert!(!impersonates_recipient(
            "List",
            "list@lists.example",
            &["list@lists.example"]
        ));
        // Same domain: an alias on our own provider is not a stranger's look-alike.
        assert!(!impersonates_recipient(
            "x",
            "me.name.2lkxp6bb@provider.example",
            &[ME]
        ));
        // An unrelated sender, and no recipients at all.
        assert!(!impersonates_recipient(
            "Anna",
            "anna@friend.example",
            &[ME]
        ));
        assert!(!impersonates_recipient("me.name", "zzz@other.example", &[]));
        // The tail must follow a separator: this is a different local part.
        assert!(!impersonates_recipient(
            "x",
            "me.name2lkxp6bb@other.example",
            &[ME]
        ));
    }

    #[test]
    fn a_bare_first_name_recipient_only_matches_on_the_full_address() {
        let recipients = ["piotr@home.example"];
        assert!(!impersonates_recipient(
            "piotr",
            "piotr.2lkxp6bb@other.example",
            &recipients
        ));
        assert!(impersonates_recipient(
            "piotr@home.example",
            "zzz@other.example",
            &recipients
        ));
    }

    #[test]
    fn random_tails() {
        for random in ["2lkxp6bb.fnvrb", "fskpfqvf.eyvxl", "k2oypgxx", "12345"] {
            assert!(looks_random(random), "{random}");
        }
        for human in ["work", "kowalski", "newsletter", "ab1", "sales.team", "a-b"] {
            assert!(!looks_random(human), "{human}");
        }
    }

    #[test]
    fn wp_dkim_status_maps_to_a_result() {
        assert_eq!(parse_wp_dkim("good (id: sl25.example)"), AuthVerdict::Pass);
        assert_eq!(parse_wp_dkim("GOOD (id: x)"), AuthVerdict::Pass);
        assert_eq!(parse_wp_dkim("no signature (id: n/a)"), AuthVerdict::None);
        assert_eq!(parse_wp_dkim("bad (id: x)"), AuthVerdict::Fail);
        assert_eq!(parse_wp_dkim("invalid (id: x)"), AuthVerdict::Fail);
        assert_eq!(parse_wp_dkim(""), AuthVerdict::None);
    }

    #[test]
    fn wp_spam_header_maps_to_a_verdict() {
        assert_eq!(parse_wp_spam("YES (U9) 0000019 [IAYE]"), Some(true));
        assert_eq!(parse_wp_spam("NO"), Some(false));
        assert_eq!(parse_wp_spam("no (x)"), Some(false));
        assert_eq!(parse_wp_spam("MAYBE"), None);
        assert_eq!(parse_wp_spam(""), None);
    }

    fn with_headers(extra: &str, provider: Option<ProviderHeaders>) -> ParsedMessage {
        let raw =
            format!("{extra}From: a@b.example\r\nTo: me@x.example\r\nSubject: s\r\n\r\nbody\r\n");
        parse_message(raw.as_bytes(), None, provider).unwrap()
    }

    #[test]
    fn provider_headers_are_ignored_unless_enabled() {
        let headers = "X-WP-DKIM-Status: good (id: b.example)\r\nX-WP-SPAM: YES (U9)\r\n";
        let off = with_headers(headers, None);
        assert_eq!(off.auth.dkim, AuthVerdict::None);
        assert_eq!(off.provider_spam, None);
        let on = with_headers(headers, Some(ProviderHeaders::Wp));
        assert_eq!(on.auth.dkim, AuthVerdict::Pass);
        assert_eq!(on.provider_spam, Some(true));
    }

    #[test]
    fn authentication_results_wins_over_provider_headers() {
        let headers = "Authentication-Results: mx.test; dkim=fail\r\n\
                       X-WP-DKIM-Status: good (id: b.example)\r\n";
        let message = with_headers(headers, Some(ProviderHeaders::Wp));
        assert_eq!(message.auth.dkim, AuthVerdict::Fail);
    }
}
