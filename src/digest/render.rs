//! From the digest model to text: a plain-text part, an HTML part and the finished message.
//!
//! [`render`] is a pure function from a [`Digest`] to two strings, so tests compare whole
//! strings. [`compose`] wraps them into a `multipart/alternative` message with the headers;
//! it is pure too (the time and the Message-ID are passed in), but its output contains a MIME
//! boundary chosen by the library, so tests read it back instead of comparing it.
//!
//! # The HTML
//!
//! One small document with an inline `<style>` block: system fonts, `color-scheme: light dark`
//! and a dark-mode media query, so it reads well in both. It loads nothing from the network
//! (no images, fonts or external CSS), so opening the digest tells nobody anything.
//!
//! **Everything that came from email is escaped** by [`escape_html`]: senders, subjects and the
//! model's reasons. The model's reasons are themselves derived from hostile input.

use std::io;

use jiff::civil;
use mail_builder::MessageBuilder;
use mail_builder::headers::raw::Raw;

use super::build::{Digest, Item, Section, clean_line};

/// The `X-Lets-Triage-Mail` header value that marks a digest, for people and mail filters.
///
/// The service itself does **not** trust this header to recognise its own digests: a sender
/// can write any header. See [`crate::digest`].
pub const DIGEST_HEADER_VALUE: &str = "digest";

/// The longest subject we write.
const MAX_SUBJECT_CHARS: usize = 150;

/// A rendered digest, before it is wrapped into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub subject: String,
    pub text: String,
    pub html: String,
}

/// Renders the digest. Pure.
pub fn render(digest: &Digest) -> Rendered {
    Rendered {
        subject: subject(digest),
        text: render_text(digest),
        html: render_html(digest),
    }
}

/// `Daily triage — <account> — <date>`, as one safe line.
///
/// The account name comes from the user's own config, but the line goes into a mail header,
/// so it is cleaned anyway: a name with a line break must never be able to add a header.
fn subject(digest: &Digest) -> String {
    clean_line(
        &format!("Daily triage — {} — {}", digest.account, digest.date),
        MAX_SUBJECT_CHARS,
    )
}

// ---------------------------------------------------------------------------------------------
// Shared wording
// ---------------------------------------------------------------------------------------------

/// "Monday 26 October 2026".
fn long_date(date: civil::Date) -> String {
    date.strftime("%A %-d %B %Y").to_string()
}

/// "25 Oct 07:00".
fn short_time(time: civil::DateTime) -> String {
    time.strftime("%-d %b %H:%M").to_string()
}

fn period_line(digest: &Digest) -> String {
    format!(
        "Covers {} to {}, {} {}.",
        short_time(digest.from),
        short_time(digest.to),
        digest.total,
        plural(digest.total, "message", "messages"),
    )
}

fn plural<'a>(count: usize, one: &'a str, many: &'a str) -> &'a str {
    if count == 1 { one } else { many }
}

const DRY_RUN_NOTE: &str = "Dry-run: nothing was flagged on the server. Any flags below are the ones that would have been set.";

/// The one-line headline: how many messages need the user.
fn headline(digest: &Digest) -> String {
    match digest.needs_you() {
        0 if digest.total == 0 => "No new mail was classified in this period.".to_owned(),
        0 => "Nothing needed your attention.".to_owned(),
        n => format!("{n} {} you.", plural(n, "message needs", "messages need")),
    }
}

/// "7 newsletters, 3 marketing, 2 cold outreach", without the zero parts.
fn bulk_breakdown(digest: &Digest) -> String {
    let noise = &digest.noise;
    let parts = [
        (noise.newsletters, "newsletters"),
        (noise.marketing, "marketing"),
        (noise.cold_outreach, "cold outreach"),
    ];
    join_counts(&parts)
}

fn junk_breakdown(digest: &Digest) -> String {
    let parts = [
        (digest.noise.spam, "spam"),
        (digest.noise.phishing, "phishing"),
    ];
    join_counts(&parts)
}

fn join_counts(parts: &[(usize, &str)]) -> String {
    parts
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, name)| format!("{count} {name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// "alice@example.org (5), promo@shop.com (3)".
fn noisy_senders(digest: &Digest) -> String {
    digest
        .noise
        .top_senders
        .iter()
        .map(|s| format!("{} ({})", s.sender, s.count))
        .collect::<Vec<_>>()
        .join(", ")
}

/// "due 2026-11-30", or nothing.
fn due(item: &Item) -> Option<String> {
    item.deadline.map(|date| format!("due {date}"))
}

fn hidden_line(section: &Section) -> Option<String> {
    (section.hidden > 0).then(|| format!("and {} more", section.hidden))
}

const FOOTER: &str = "Written by Let's Triage Mail on your own machine. Nothing left it.";

// ---------------------------------------------------------------------------------------------
// Plain text
// ---------------------------------------------------------------------------------------------

fn render_text(digest: &Digest) -> String {
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };

    line(&format!("Daily triage: {}", digest.account));
    line(&long_date(digest.date));
    line(&period_line(digest));
    if digest.dry_run {
        line("");
        line(DRY_RUN_NOTE);
    }
    line("");
    line(&headline(digest));

    for section in &digest.sections {
        line("");
        line(&format!("{} ({})", section.title, section.total()));
        for item in &section.items {
            let mut head = format!("- {}: {}", item.sender, item.subject);
            if let Some(due) = due(item) {
                head.push_str(&format!(" ({due})"));
            }
            line(&head);
            let mut detail = item.reason.clone();
            if !detail.is_empty() {
                detail.push(' ');
            }
            detail.push_str(&format!("[{}]", short_time(item.received)));
            line(&format!("  {detail}"));
        }
        if let Some(more) = hidden_line(section) {
            line(&format!("  ... {more}"));
        }
    }

    let noise = &digest.noise;
    if noise.bulk() + noise.junk() + noise.other > 0 {
        line("");
        line("The rest");
        if noise.bulk() > 0 {
            line(&format!("- Bulk mail: {}", bulk_breakdown(digest)));
        }
        if !noise.top_senders.is_empty() {
            line(&format!("- Noisiest senders: {}", noisy_senders(digest)));
        }
        if noise.junk() > 0 {
            line(&format!("- Junk: {}", junk_breakdown(digest)));
        }
        if noise.other > 0 {
            line(&format!("- Other mail: {} (nothing to do)", noise.other));
        }
    }

    line("");
    line(FOOTER);
    out
}

// ---------------------------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------------------------

/// Escapes text for use inside HTML, in an element or in a quoted attribute.
///
/// Anything that came from email goes through this before it reaches the HTML. The five
/// characters below are the whole job: with them escaped, text cannot open a tag, close an
/// attribute or start an entity.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

const HTML_STYLE: &str = "\
body { font: 15px/1.45 -apple-system, BlinkMacSystemFont, 'Segoe UI', Helvetica, Arial, sans-serif; \
color: #1d1d1f; background: #ffffff; margin: 0; padding: 16px; }
.wrap { max-width: 640px; margin: 0 auto; }
h1 { font-size: 20px; margin: 0 0 2px; }
h2 { font-size: 16px; margin: 22px 0 6px; }
.muted { color: #6e6e73; }
.note { padding: 8px 10px; border-radius: 6px; background: #fff4d6; color: #5c4400; }
ul { margin: 0; padding-left: 20px; }
li { margin: 0 0 8px; }
@media (prefers-color-scheme: dark) {
  body { color: #f5f5f7; background: #1c1c1e; }
  .muted { color: #a1a1a6; }
  .note { background: #3d3216; color: #f2d98a; }
}";

fn render_html(digest: &Digest) -> String {
    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<meta name=\"color-scheme\" content=\"light dark\">\n");
    out.push_str("<meta name=\"supported-color-schemes\" content=\"light dark\">\n");
    out.push_str(&format!(
        "<title>{}</title>\n",
        escape_html(&subject(digest))
    ));
    out.push_str(&format!(
        "<style>\n{HTML_STYLE}\n</style>\n</head>\n<body>\n<div class=\"wrap\">\n"
    ));

    out.push_str(&format!(
        "<h1>Daily triage: {}</h1>\n<div class=\"muted\">{}<br>{}</div>\n",
        escape_html(&digest.account),
        escape_html(&long_date(digest.date)),
        escape_html(&period_line(digest)),
    ));
    if digest.dry_run {
        out.push_str(&format!(
            "<p class=\"note\">{}</p>\n",
            escape_html(DRY_RUN_NOTE)
        ));
    }
    out.push_str(&format!(
        "<p><strong>{}</strong></p>\n",
        escape_html(&headline(digest))
    ));

    for section in &digest.sections {
        out.push_str(&format!(
            "<h2>{} ({})</h2>\n<ul>\n",
            escape_html(&section.title),
            section.total()
        ));
        for item in &section.items {
            let mut head = format!(
                "<strong>{}</strong>: {}",
                escape_html(&item.sender),
                escape_html(&item.subject)
            );
            if let Some(due) = due(item) {
                head.push_str(&format!(" ({})", escape_html(&due)));
            }
            let mut detail = escape_html(&item.reason);
            if !detail.is_empty() {
                detail.push(' ');
            }
            detail.push_str(&format!("[{}]", escape_html(&short_time(item.received))));
            out.push_str(&format!(
                "<li>{head}<br><span class=\"muted\">{detail}</span></li>\n"
            ));
        }
        if let Some(more) = hidden_line(section) {
            out.push_str(&format!(
                "<li class=\"muted\">{}</li>\n",
                escape_html(&more)
            ));
        }
        out.push_str("</ul>\n");
    }

    let noise = &digest.noise;
    if noise.bulk() + noise.junk() + noise.other > 0 {
        out.push_str("<h2>The rest</h2>\n<ul>\n");
        let mut row = |text: String| {
            out.push_str(&format!("<li>{}</li>\n", escape_html(&text)));
        };
        if noise.bulk() > 0 {
            row(format!("Bulk mail: {}", bulk_breakdown(digest)));
        }
        if !noise.top_senders.is_empty() {
            row(format!("Noisiest senders: {}", noisy_senders(digest)));
        }
        if noise.junk() > 0 {
            row(format!("Junk: {}", junk_breakdown(digest)));
        }
        if noise.other > 0 {
            row(format!("Other mail: {} (nothing to do)", noise.other));
        }
        out.push_str("</ul>\n");
    }

    out.push_str(&format!(
        "<p class=\"muted\">{}</p>\n</div>\n</body>\n</html>\n",
        escape_html(FOOTER)
    ));
    out
}

// ---------------------------------------------------------------------------------------------
// The message
// ---------------------------------------------------------------------------------------------

/// Who the digest is from and to, and its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope<'a> {
    /// The display name of the sender, such as "Let's Triage Mail".
    pub from_name: &'a str,
    pub from_address: &'a str,
    /// The account's own address.
    pub to_address: &'a str,
    /// The `Message-ID` without angle brackets. Random, see [`crate::digest::deliver`].
    pub message_id: &'a str,
    /// The `Date` header, as Unix seconds.
    pub date: i64,
}

/// Builds the `multipart/alternative` message (plain text first, HTML second, so clients show
/// the richer part), as bytes with CRLF line endings, ready for `APPEND`.
///
/// Header values that come from the user's config or from email pass through
/// [`clean_line`], so none of them can contain a line break and start a new header.
pub fn compose(rendered: &Rendered, envelope: &Envelope<'_>) -> io::Result<Vec<u8>> {
    MessageBuilder::new()
        .from((
            clean_line(envelope.from_name, 100),
            clean_line(envelope.from_address, 200),
        ))
        .to(clean_line(envelope.to_address, 200))
        .subject(clean_line(&rendered.subject, MAX_SUBJECT_CHARS))
        .message_id(clean_line(envelope.message_id, 200))
        .date(envelope.date)
        // Marks the message for people and filters. The service does not rely on it.
        .header("X-Lets-Triage-Mail", Raw::new(DIGEST_HEADER_VALUE))
        // RFC 3834: tells autoresponders and mailing-list software not to answer it.
        .header("Auto-Submitted", Raw::new("auto-generated"))
        .text_body(rendered.text.clone())
        .html_body(rendered.html.clone())
        .write_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::build::{Noise, SenderCount};
    use crate::engine::TriageFlag;

    fn item(sender: &str, subject: &str, reason: &str) -> Item {
        Item {
            sender: sender.into(),
            subject: subject.into(),
            reason: reason.into(),
            deadline: None,
            received: civil::date(2026, 10, 25).at(14, 2, 0, 0),
        }
    }

    fn section(flag: TriageFlag, title: &str, items: Vec<Item>) -> Section {
        Section {
            flag,
            title: title.into(),
            items,
            hidden: 0,
        }
    }

    /// A digest with every kind of content.
    fn full_digest() -> Digest {
        let mut due_item = item("billing@power.example", "Invoice 77", "An unpaid invoice.");
        due_item.deadline = Some(civil::date(2026, 11, 30));
        Digest {
            account: "work".into(),
            date: civil::date(2026, 10, 26),
            from: civil::date(2026, 10, 25).at(7, 0, 0, 0),
            to: civil::date(2026, 10, 26).at(7, 0, 0, 0),
            dry_run: false,
            total: 57,
            sections: vec![
                section(
                    TriageFlag::ReplyNeeded,
                    "Reply needed",
                    vec![item(
                        "alice@example.org",
                        "Contract draft",
                        "Asks you to confirm the date.",
                    )],
                ),
                Section {
                    hidden: 3,
                    ..section(TriageFlag::Deadline, "Deadline", vec![due_item])
                },
            ],
            noise: Noise {
                newsletters: 7,
                marketing: 3,
                cold_outreach: 2,
                spam: 4,
                phishing: 1,
                other: 5,
                top_senders: vec![
                    SenderCount {
                        sender: "promo@shop.example".into(),
                        count: 5,
                    },
                    SenderCount {
                        sender: "news@paper.example".into(),
                        count: 3,
                    },
                ],
            },
        }
    }

    #[test]
    fn the_text_part_of_a_full_digest() {
        let expected = "\
Daily triage: work
Monday 26 October 2026
Covers 25 Oct 07:00 to 26 Oct 07:00, 57 messages.

5 messages need you.

Reply needed (1)
- alice@example.org: Contract draft
  Asks you to confirm the date. [25 Oct 14:02]

Deadline (4)
- billing@power.example: Invoice 77 (due 2026-11-30)
  An unpaid invoice. [25 Oct 14:02]
  ... and 3 more

The rest
- Bulk mail: 7 newsletters, 3 marketing, 2 cold outreach
- Noisiest senders: promo@shop.example (5), news@paper.example (3)
- Junk: 4 spam, 1 phishing
- Other mail: 5 (nothing to do)

Written by Let's Triage Mail on your own machine. Nothing left it.
";
        assert_eq!(render(&full_digest()).text, expected);
    }

    #[test]
    fn the_subject_names_the_account_and_the_date() {
        assert_eq!(
            render(&full_digest()).subject,
            "Daily triage — work — 2026-10-26"
        );
    }

    #[test]
    fn a_quiet_day_says_so() {
        let mut digest = full_digest();
        digest.sections.clear();
        digest.noise = Noise {
            newsletters: 2,
            ..Noise::default()
        };
        digest.total = 2;
        let text = render(&digest).text;
        assert!(text.contains("Nothing needed your attention."), "{text}");
        assert!(text.contains("- Bulk mail: 2 newsletters"), "{text}");

        digest.noise = Noise::default();
        digest.total = 0;
        let text = render(&digest).text;
        assert!(text.contains("No new mail was classified"), "{text}");
        assert!(!text.contains("The rest"), "{text}");
    }

    #[test]
    fn a_dry_run_digest_says_the_flags_were_not_set() {
        let mut digest = full_digest();
        digest.dry_run = true;
        let rendered = render(&digest);
        assert!(rendered.text.contains("Dry-run: nothing was flagged"));
        assert!(rendered.html.contains("Dry-run: nothing was flagged"));
    }

    #[test]
    fn the_html_has_no_remote_content_and_supports_dark_mode() {
        let html = render(&full_digest()).html;
        for remote in [
            "http://", "https://", "<img", "<link", "<script", "@import", "url(",
        ] {
            assert!(!html.contains(remote), "found {remote} in the HTML");
        }
        assert!(html.contains("prefers-color-scheme: dark"));
        assert!(html.contains("color-scheme"));
    }

    #[test]
    fn everything_from_email_is_escaped_in_the_html() {
        let mut digest = full_digest();
        digest.sections = vec![section(
            TriageFlag::ReplyNeeded,
            "Reply <b>needed</b>",
            vec![item(
                "evil\"><script>alert(1)</script>@x.com",
                "<script>alert('subject')</script> & more",
                "<img src=x onerror=alert(1)>",
            )],
        )];
        digest.noise.top_senders[0].sender = "<script>top</script>".into();
        digest.account = "<script>acct</script>".into();

        let html = render(&digest).html;
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(&#39;subject&#39;)&lt;/script&gt; &amp; more"),
            "{html}"
        );
        assert!(
            html.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "{html}"
        );
        assert!(html.contains("evil&quot;&gt;&lt;script&gt;"), "{html}");
    }

    #[test]
    fn escape_html_handles_the_five_special_characters() {
        assert_eq!(
            escape_html("a & b < c > d \" e ' f"),
            "a &amp; b &lt; c &gt; d &quot; e &#39; f"
        );
        assert_eq!(escape_html("plain ä"), "plain ä");
    }

    /// The trimmed values of every header with this name.
    fn header_values(message: &mail_parser::Message<'_>, name: &str) -> Vec<String> {
        message
            .headers()
            .all(name)
            .map(|h| String::from_utf8_lossy(h.raw_value()).trim().to_owned())
            .collect()
    }

    fn envelope() -> Envelope<'static> {
        Envelope {
            from_name: "Let's Triage Mail",
            from_address: "lets-triage-mail@localhost",
            to_address: "me@example.org",
            message_id: "0123abcd@lets-triage-mail.invalid",
            date: 1_792_998_000,
        }
    }

    #[test]
    fn the_message_has_both_parts_and_our_headers() {
        let rendered = render(&full_digest());
        let bytes = compose(&rendered, &envelope()).unwrap();
        let message = mail_parser::MessageParser::default().parse(&bytes).unwrap();

        assert_eq!(message.subject(), Some("Daily triage — work — 2026-10-26"));
        assert_eq!(
            message.message_id(),
            Some("0123abcd@lets-triage-mail.invalid")
        );
        assert_eq!(
            message
                .from()
                .and_then(|a| a.first())
                .and_then(|a| a.address()),
            Some("lets-triage-mail@localhost")
        );
        assert_eq!(
            message
                .to()
                .and_then(|a| a.first())
                .and_then(|a| a.address()),
            Some("me@example.org")
        );
        assert_eq!(
            message.date().map(|d| d.to_timestamp()),
            Some(1_792_998_000)
        );
        assert_eq!(header_values(&message, "X-Lets-Triage-Mail"), ["digest"]);
        assert_eq!(message.text_body().len(), 1);
        assert_eq!(message.html_body().len(), 1);
        assert_eq!(
            message.body_text(0).map(|t| t.replace("\r\n", "\n")),
            Some(rendered.text)
        );
        let raw = String::from_utf8_lossy(&bytes);
        assert!(raw.contains("multipart/alternative"), "{raw}");
    }

    #[test]
    fn a_line_break_in_the_account_name_cannot_add_a_header() {
        let mut digest = full_digest();
        digest.account = "work\r\nBcc: spy@evil.example".into();
        let rendered = render(&digest);
        assert!(!rendered.subject.contains('\n'));
        assert!(!rendered.subject.contains('\r'));

        let mut env = envelope();
        env.to_address = "me@example.org\r\nBcc: spy@evil.example";
        let bytes = compose(&rendered, &env).unwrap();
        let message = mail_parser::MessageParser::default().parse(&bytes).unwrap();
        assert!(header_values(&message, "Bcc").is_empty());
    }
}
