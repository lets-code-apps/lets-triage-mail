//! From the digest model to text: a plain-text part, an HTML part and the finished message.
//!
//! [`render`] is a pure function from a [`Digest`] to two strings, so tests compare whole
//! strings. [`compose`] wraps them into a message with the headers; it is pure too (the time
//! and the Message-ID are passed in), but its output contains a MIME boundary chosen by the
//! library, so tests read it back instead of comparing it.
//!
//! # The HTML
//!
//! The design is the "let's code" digest email (`design/email/triage-mail/` in the design
//! language repo, written by its `build.py`). It is email-safe HTML: tables for layout, inline
//! styles in the light theme, one `<style>` block with the dark theme, the system font stack.
//! Colours are the design tokens written out, because mail clients don't support CSS
//! variables reliably.
//!
//! The only image is the header (`assets/digest-header.jpg`), sent **inside the message** as
//! an inline part (`cid:digest-header`), not as a link: the digest loads nothing from the
//! network, so opening it tells nobody anything. A CSS gradient under it shows when a client
//! blocks images anyway.
//!
//! **Everything that came from email is escaped** by [`escape_html`]: senders, subjects and the
//! model's reasons. The model's reasons are themselves derived from hostile input.

use std::borrow::Cow;
use std::io;

use jiff::civil;
use mail_builder::MessageBuilder;
use mail_builder::headers::raw::Raw;
use mail_builder::mime::{BodyPart, MimePart};

use super::build::{Digest, Item, Section, clean_line};
use crate::flags::FlagColour;

/// The `X-Lets-Triage-Mail` header value that marks a digest, for people and mail filters.
///
/// The service itself does **not** trust this header to recognise its own digests: a sender
/// can write any header. See [`crate::digest`].
pub const DIGEST_HEADER_VALUE: &str = "digest";

/// The longest subject we write.
const MAX_SUBJECT_CHARS: usize = 150;

/// The header image, from the design repo (`render-header.html`): the grain with the emblem.
const HEADER_IMAGE: &[u8] = include_bytes!("../../assets/digest-header.jpg");
/// Its `Content-ID`; the HTML refers to it as `cid:digest-header`.
const HEADER_CID: &str = "digest-header";

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

/// "Tue 27 Oct 2026".
fn long_date(date: civil::Date) -> String {
    date.strftime("%a %-d %b %Y").to_string()
}

/// "26 Oct 07:00".
fn short_time(time: civil::DateTime) -> String {
    time.strftime("%-d %b %H:%M").to_string()
}

/// When a message came, relative to the digest's date: "06:12", "yesterday 21:03", "sat 11:20".
/// A digest covers at most 48 hours, so a weekday is never ambiguous.
fn when(time: civil::DateTime, today: civil::Date) -> String {
    let clock = time.strftime("%H:%M");
    if time.date() == today {
        clock.to_string()
    } else if today.yesterday().ok() == Some(time.date()) {
        format!("yesterday {clock}")
    } else {
        time.strftime("%a %H:%M").to_string().to_lowercase()
    }
}

fn count_of(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// "26 Oct 07:00", "27 Oct 07:00", "57 messages".
fn period_parts(digest: &Digest) -> (String, String, String) {
    (
        short_time(digest.from),
        short_time(digest.to),
        count_of(digest.total, "message", "messages"),
    )
}

/// The headline, lowercase as a heading: "5 messages need you", "nothing needs you".
fn headline(digest: &Digest) -> String {
    match digest.needs_you() {
        0 if digest.total == 0 => "no new mail".to_owned(),
        0 => "nothing needs you".to_owned(),
        n if digest.dry_run => count_of(
            n,
            "message would have been flagged",
            "messages would have been flagged",
        ),
        n => count_of(n, "message needs you", "messages need you"),
    }
}

/// The hidden line under the subject in the inbox list: what is in the digest.
fn preheader(digest: &Digest) -> String {
    match digest.needs_you() {
        0 if digest.total == 0 => "No new mail.".to_owned(),
        0 => format!(
            "Nothing needs you. {} sorted.",
            count_of(digest.total, "message", "messages")
        ),
        n if digest.dry_run => format!(
            "Dry run: {}",
            count_of(
                n,
                "message would have been flagged",
                "messages would have been flagged"
            )
        ),
        _ => digest
            .sections
            .iter()
            .map(|s| format!("{} {}", s.total(), s.title.to_lowercase()))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// The note under the header, as (pill, text), when the flags in the digest were not set.
fn note(digest: &Digest) -> Option<(&'static str, &'static str)> {
    if digest.dry_run {
        Some((
            "dry run",
            "Nothing was flagged in Mail. This digest shows what would have happened and was saved as a .eml file, not sent.",
        ))
    } else if digest.classify_only {
        Some((
            "flags off",
            "This account is sorted but never flagged, so this digest is the only place these messages show up.",
        ))
    } else {
        None
    }
}

/// "due Fri 30 Oct", or nothing.
fn due(item: &Item) -> Option<String> {
    item.deadline
        .map(|date| format!("due {}", date.strftime("%a %-d %b")))
}

/// "and 31 more, flagged in Mail": the flags were set, so Mail's Flagged mailbox has them all.
fn more_line(digest: &Digest, hidden: usize) -> String {
    if digest.dry_run || digest.classify_only {
        format!("and {hidden} more")
    } else {
        format!("and {hidden} more, flagged in Mail")
    }
}

const EMPTY_DAY: &str = "Nothing came in during this period, so there was nothing to sort.";
const FOOTER: &str = "Written by Let's Triage Mail on your own machine. Nothing left it.";

const MISFILED_TITLE: &str = "possibly misfiled in spam";
const MISFILED_NOTE: &str = "People you know, with a valid signature, whose mail is in your Spam folder. Nothing was moved.";

/// The rows of "the rest": label, what it holds, count.
fn rest_rows(digest: &Digest) -> [(&'static str, &'static str, usize); 3] {
    let noise = &digest.noise;
    [
        (
            "bulk mail",
            "newsletters, marketing, cold outreach",
            noise.bulk(),
        ),
        ("junk", "spam and phishing", noise.junk()),
        ("other mail", "nothing to do", noise.other),
    ]
}

// ---------------------------------------------------------------------------------------------
// Plain text
// ---------------------------------------------------------------------------------------------

/// Capitalises the first letter, for a heading used as a sentence.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

fn render_text(digest: &Digest) -> String {
    let mut out = String::new();
    let mut line = |text: &str| {
        out.push_str(text);
        out.push('\n');
    };

    let (from, to, messages) = period_parts(digest);
    line(&format!("DAILY TRIAGE: {}", digest.account));
    line(&long_date(digest.date));
    line(&format!("{from} to {to}, {messages}"));
    if let Some((pill, text)) = note(digest) {
        line("");
        line(&format!("{}: {text}", pill.to_uppercase()));
    }
    line("");
    line(&format!("{}.", sentence(&headline(digest))));

    let item_lines = |line: &mut dyn FnMut(&str), item: &Item| {
        let mut head = format!("- {}, {}", item.sender, when(item.received, digest.date));
        if let Some(due) = due(item) {
            head.push_str(&format!(", {due}"));
        }
        line(&head);
        line(&format!("  {}", item.subject));
        if !item.reason.is_empty() {
            line(&format!("  {}", item.reason));
        }
    };

    for section in &digest.sections {
        line("");
        line(&format!(
            "{} ({})",
            section.title.to_uppercase(),
            section.total()
        ));
        for item in &section.items {
            item_lines(&mut line, item);
        }
        if section.hidden > 0 {
            line(&format!("  {}", more_line(digest, section.hidden)));
        }
    }

    if !digest.misfiled.is_empty() {
        line("");
        line(&format!(
            "{} ({})",
            MISFILED_TITLE.to_uppercase(),
            digest.misfiled.len() + digest.misfiled_hidden
        ));
        line(MISFILED_NOTE);
        for item in &digest.misfiled {
            item_lines(&mut line, item);
        }
        if digest.misfiled_hidden > 0 {
            line(&format!("  and {} more", digest.misfiled_hidden));
        }
    }

    line("");
    if digest.total == 0 {
        line(EMPTY_DAY);
    } else {
        line("THE REST");
        for (label, holds, count) in rest_rows(digest) {
            line(&format!("{}: {count} ({holds})", sentence(label)));
        }
        if !digest.noise.top_senders.is_empty() {
            let senders = digest
                .noise
                .top_senders
                .iter()
                .map(|s| format!("{} {}", s.sender, s.count))
                .collect::<Vec<_>>()
                .join(", ");
            line(&format!("Noisiest senders: {senders}"));
        }
    }

    line("");
    line("--");
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

// Token values from the design repo's `tokens/tokens.json`, written out. Light ones go inline;
// the dark ones are in `DARK_CSS`, by class.
const FONT: &str =
    "Poppins,-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
const BG: &str = "#F5F5F7";
const SURFACE: &str = "#FFFFFF";
const RAISED: &str = "#EDEDF1";
const LINE: &str = "#DEDEE4";
const FG: &str = "#0B0B0E";
const MUTED: &str = "#5C5C66";

const DARK_CSS: &str = "\
body{background:#0B0B0E !important}
.c-bg{background:#0B0B0E !important}.c-surface{background:#141418 !important}.c-raised{background:#1C1C22 !important}
.c-line{border-color:#2A2A31 !important}.c-fg{color:#F4F4F6 !important}.c-muted{color:#A0A0AA !important}
.d-orange{background:#FF9F0A !important}.d-yellow{background:#FFD60A !important}.d-green{background:#30D158 !important}
.d-purple{background:#BF5AF2 !important}.d-gray{background:#8E8E93 !important}.d-neutral{background:#A0A0AA !important}";

/// The dot's tone and its light colour. Apple Mail's flag colours (`color.palette.flag`);
/// "neutral" is for notes that aren't a flag. Blue has no token yet, so it gets the neutral
/// dot; red is never set by the service.
fn tone(colour: Option<FlagColour>) -> (&'static str, &'static str) {
    match colour {
        Some(FlagColour::Orange) => ("orange", "#FF9500"),
        Some(FlagColour::Yellow) => ("yellow", "#FFCC00"),
        Some(FlagColour::Green) => ("green", "#34C759"),
        Some(FlagColour::Purple) => ("purple", "#AF52DE"),
        Some(FlagColour::Gray) => ("gray", "#8E8E93"),
        _ => ("neutral", MUTED),
    }
}

fn dot(colour: Option<FlagColour>) -> String {
    let (name, value) = tone(colour);
    format!(
        "<span class=\"d-{name}\" aria-hidden=\"true\" style=\"display:inline-block;width:7px;height:7px;border-radius:50%;background:{value};vertical-align:middle\"></span>"
    )
}

/// A pill; `text` must already be escaped.
fn pill(text: &str, colour: Option<FlagColour>) -> String {
    format!(
        "<span class=\"c-raised c-fg\" style=\"display:inline-block;padding:4px 10px;border-radius:999px;background:{RAISED};color:{FG};font:500 12px/16px {FONT};white-space:nowrap\">{}&nbsp; {text}</span>",
        dot(colour)
    )
}

/// The header: the grain image with the emblem, the lockup as live text over a scrim, and an
/// ember gradient under the image for clients that block it.
fn header_html() -> String {
    format!(
        "<tr><td height=\"152\" style=\"border-radius:24px;background-color:#24150F;background-image:linear-gradient(90deg,rgba(0,0,0,0.55) 0,rgba(0,0,0,0.55) 130px,rgba(0,0,0,0) 210px),url(cid:{HEADER_CID}),radial-gradient(120% 140% at 100% 0%,#D98A45 0%,#B8612E 22%,rgba(107,58,34,0) 60%),linear-gradient(110deg,#24150F 0%,#24150F 40%,#6B3A22 100%);background-size:auto,cover,auto,auto;background-position:0 0,right center,0 0,0 0;background-repeat:no-repeat;padding:24px;color:#FFFFFF;vertical-align:top\">
<div style=\"font:400 17px/1.05 {FONT};color:#FFFFFF\">let&rsquo;s</div>
<div style=\"font:600 17px/1.15 {FONT};letter-spacing:-0.01em;color:#FFFFFF\">triage mail</div>
<div style=\"height:84px;line-height:84px;font-size:1px\">&nbsp;</div>
<div style=\"font:600 26px/1.15 {FONT};letter-spacing:-0.01em;color:#FFFFFF\">daily triage</div>
</td></tr>"
    )
}

fn note_html(pill_text: &str, text: &str) -> String {
    format!(
        "<tr><td style=\"padding:24px 0 0\"><div class=\"c-raised\" style=\"border-radius:14px;background:{RAISED};padding:14px 16px\">
{}
<div class=\"c-fg\" style=\"font:400 15px/1.5 {FONT};color:{FG};padding-top:8px\">{}</div></div></td></tr>",
        pill(&escape_html(pill_text), None),
        escape_html(text)
    )
}

fn headline_html(digest: &Digest) -> String {
    let (from, to, messages) = period_parts(digest);
    format!(
        "<tr><td style=\"padding:32px 4px 0\">
<div dir=\"auto\" class=\"c-muted\" style=\"font:500 13px/1.4 {FONT};color:{MUTED};padding-bottom:6px;word-break:break-word\">{} &middot; {}</div>
<div class=\"c-fg\" style=\"font:600 26px/1.15 {FONT};letter-spacing:-0.01em;color:{FG}\">{}</div>
<div class=\"c-muted\" style=\"font:400 13px/1.4 {FONT};color:{MUTED};padding-top:6px;font-variant-numeric:tabular-nums\">{}</div>
</td></tr>",
        escape_html(&digest.account),
        escape_html(&long_date(digest.date).to_lowercase()),
        escape_html(&headline(digest)),
        escape_html(&format!("{from} – {to} · {messages}").to_lowercase()),
    )
}

/// The row of count tiles, one per flag.
fn strip_html(sections: &[Section]) -> String {
    let tiles: String = sections
        .iter()
        .map(|s| {
            format!(
                "<div class=\"c-surface c-line\" style=\"display:inline-block;vertical-align:top;min-width:96px;margin:0 8px 8px 0;padding:12px 16px;border-radius:14px;border:1px solid {LINE};background:{SURFACE}\">
<div class=\"c-fg\" style=\"font:600 26px/1.15 {FONT};color:{FG};font-variant-numeric:tabular-nums\">{}</div>
<div class=\"c-fg\" style=\"font:500 12px/16px {FONT};color:{FG};padding-top:4px;white-space:nowrap\">{}&nbsp; {}</div></div>",
                s.total(),
                dot(Some(s.colour)),
                escape_html(&s.title.to_lowercase()),
            )
        })
        .collect();
    format!("<tr><td style=\"padding:24px 0 0;font-size:0\">{tiles}</td></tr>")
}

/// One or two letters for the avatar: the initials of a name, or the first letter of an address.
fn initials(sender: &str) -> String {
    let letters: String = sender
        .split_whitespace()
        .filter_map(|word| word.trim_start_matches('"').chars().next())
        .filter(|c| c.is_alphabetic())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".to_owned()
    } else {
        letters
    }
}

fn row_html(digest: &Digest, item: &Item, colour: Option<FlagColour>, last: bool) -> String {
    let border = if last {
        String::new()
    } else {
        format!("border-bottom:1px solid {LINE};")
    };
    let reason = if item.reason.is_empty() {
        String::new()
    } else {
        format!(
            "<div class=\"c-muted\" dir=\"auto\" style=\"font:400 13px/1.4 {FONT};color:{MUTED};padding-top:4px\">{}</div>",
            escape_html(&item.reason)
        )
    };
    let due = due(item).map_or_else(String::new, |due| {
        format!(
            "<div style=\"padding-top:8px\">{}</div>",
            pill(&escape_html(&due.to_lowercase()), colour)
        )
    });
    format!(
        "<tr><td class=\"c-line\" style=\"{border}padding:14px 16px\">
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\"><tr>
<td width=\"36\" valign=\"top\" style=\"padding-right:14px\"><div class=\"c-raised c-fg\" style=\"width:36px;height:36px;border-radius:50%;background:{RAISED};color:{FG};font:500 13px/36px {FONT};text-align:center\">{}</div></td>
<td valign=\"top\" style=\"word-break:break-word\">
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\"><tr>
<td dir=\"auto\" class=\"c-fg\" style=\"font:600 15px/1.35 {FONT};color:{FG};word-break:break-word\">{}</td>
<td align=\"right\" valign=\"top\" class=\"c-muted\" style=\"font:400 13px/20px {FONT};color:{MUTED};white-space:nowrap;padding-left:12px;font-variant-numeric:tabular-nums\">{}</td></tr></table>
<div dir=\"auto\" class=\"c-fg\" style=\"font:400 15px/1.4 {FONT};color:{FG};padding-top:2px\">{}</div>
{reason}{due}</td></tr></table></td></tr>",
        escape_html(&initials(&item.sender)),
        escape_html(&item.sender),
        escape_html(&when(item.received, digest.date)),
        escape_html(&item.subject),
    )
}

/// A titled card of messages. `intro` is a muted line under the title; `more` a last row.
fn section_html(
    digest: &Digest,
    title: &str,
    colour: Option<FlagColour>,
    total: usize,
    intro: Option<&str>,
    items: &[Item],
    more: Option<String>,
) -> String {
    let mut rows: String = items
        .iter()
        .enumerate()
        .map(|(i, item)| row_html(digest, item, colour, i + 1 == items.len() && more.is_none()))
        .collect();
    if let Some(more) = more {
        rows.push_str(&format!(
            "<tr><td class=\"c-muted\" style=\"padding:14px 16px;font:400 13px/1.4 {FONT};color:{MUTED}\">{}</td></tr>",
            escape_html(&more)
        ));
    }
    let intro = intro.map_or_else(String::new, |text| {
        format!(
            "<div class=\"c-muted\" style=\"font:400 13px/1.4 {FONT};color:{MUTED};padding-top:4px\">{}</div>",
            escape_html(text)
        )
    });
    format!(
        "<tr><td style=\"padding:40px 4px 12px\"><span class=\"c-fg\" style=\"font:600 20px/1.25 {FONT};letter-spacing:-0.01em;color:{FG}\">{}&nbsp; {}</span>
<span class=\"c-muted\" style=\"font:400 15px {FONT};color:{MUTED};font-variant-numeric:tabular-nums\">&nbsp;{total}</span>{intro}</td></tr>
<tr><td><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" class=\"c-surface c-line\" style=\"border-collapse:separate;border-radius:14px;border:1px solid {LINE};background:{SURFACE}\">{rows}</table></td></tr>",
        dot(colour),
        escape_html(&title.to_lowercase()),
    )
}

fn rest_html(digest: &Digest) -> String {
    let senders = &digest.noise.top_senders;
    let rows = rest_rows(digest);
    let mut out: String = rows
        .iter()
        .enumerate()
        .map(|(i, (label, holds, count))| {
            let border = if i + 1 == rows.len() && senders.is_empty() {
                String::new()
            } else {
                format!("border-bottom:1px solid {LINE};")
            };
            format!(
                "<tr><td class=\"c-line\" style=\"{border}padding:12px 16px\"><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\"><tr>
<td><div class=\"c-fg\" style=\"font:500 15px/1.35 {FONT};color:{FG}\">{label}</div><div class=\"c-muted\" style=\"font:400 13px/1.4 {FONT};color:{MUTED}\">{holds}</div></td>
<td align=\"right\" class=\"c-fg\" style=\"font:600 15px {FONT};color:{FG};font-variant-numeric:tabular-nums;padding-left:12px\">{count}</td></tr></table></td></tr>"
            )
        })
        .collect();
    if !senders.is_empty() {
        let list: String = senders
            .iter()
            .map(|s| {
                format!(
                    "<tr><td dir=\"auto\" class=\"c-muted\" style=\"font:400 13px/1.6 {FONT};color:{MUTED};word-break:break-word\">{}</td>
<td align=\"right\" class=\"c-muted\" style=\"font:400 13px/1.6 {FONT};color:{MUTED};font-variant-numeric:tabular-nums;padding-left:12px\">{}</td></tr>",
                    escape_html(&s.sender),
                    s.count
                )
            })
            .collect();
        out.push_str(&format!(
            "<tr><td style=\"padding:12px 16px\"><div class=\"c-fg\" style=\"font:500 15px/1.35 {FONT};color:{FG};padding-bottom:4px\">noisiest senders</div>
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\">{list}</table></td></tr>"
        ));
    }
    format!(
        "<tr><td style=\"padding:40px 4px 12px\"><span class=\"c-fg\" style=\"font:600 20px/1.25 {FONT};letter-spacing:-0.01em;color:{FG}\">the rest</span></td></tr>
<tr><td><table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" class=\"c-surface c-line\" style=\"border-collapse:separate;border-radius:14px;border:1px solid {LINE};background:{SURFACE}\">{out}</table></td></tr>"
    )
}

fn render_html(digest: &Digest) -> String {
    let mut parts = vec![header_html()];
    if let Some((pill_text, text)) = note(digest) {
        parts.push(note_html(pill_text, text));
    }
    parts.push(headline_html(digest));
    if !digest.sections.is_empty() {
        parts.push(strip_html(&digest.sections));
    }
    for section in &digest.sections {
        let more = (section.hidden > 0).then(|| more_line(digest, section.hidden));
        parts.push(section_html(
            digest,
            &section.title,
            Some(section.colour),
            section.total(),
            None,
            &section.items,
            more,
        ));
    }
    if !digest.misfiled.is_empty() {
        let more = (digest.misfiled_hidden > 0)
            .then(|| format!("and {} more, still in Spam", digest.misfiled_hidden));
        parts.push(section_html(
            digest,
            MISFILED_TITLE,
            None,
            digest.misfiled.len() + digest.misfiled_hidden,
            Some(MISFILED_NOTE),
            &digest.misfiled,
            more,
        ));
    }
    if digest.total == 0 {
        parts.push(format!(
            "<tr><td class=\"c-fg\" style=\"padding:16px 4px 0;font:400 16px/1.55 {FONT};color:{FG};max-width:62ch\">{EMPTY_DAY}</td></tr>"
        ));
    } else {
        parts.push(rest_html(digest));
    }
    parts.push(format!(
        "<tr><td class=\"c-line c-muted\" style=\"padding:40px 4px 8px;font:400 13px/1.4 {FONT};color:{MUTED}\">Written by let&rsquo;s triage mail on your own machine. Nothing left it.</td></tr>"
    ));

    format!(
        "<!DOCTYPE html>
<html lang=\"en\">
<head>
<meta charset=\"utf-8\">
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">
<meta name=\"color-scheme\" content=\"light dark\">
<meta name=\"supported-color-schemes\" content=\"light dark\">
<title>{title}</title>
<style>
:root{{color-scheme:light dark;supported-color-schemes:light dark}}
body{{margin:0;padding:0;background:{BG}}}
@media (prefers-color-scheme: dark){{
{DARK_CSS}
}}
</style>
</head>
<body class=\"c-bg\">
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" class=\"c-bg\" style=\"background:{BG}\"><tr><td align=\"center\" style=\"padding:24px 16px\">
<div style=\"display:none;max-height:0;overflow:hidden\">{preheader}</div>
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"max-width:600px;text-align:left\">
{parts}
</table></td></tr></table>
</body>
</html>
",
        title = escape_html(&subject(digest)),
        preheader = escape_html(&preheader(digest)),
        parts = parts.join("\n"),
    )
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

/// Builds the message as bytes with CRLF line endings, ready for `APPEND`:
///
/// ```text
/// multipart/alternative
///   text/plain
///   multipart/related
///     text/html
///     image/jpeg  (inline, Content-ID <digest-header>)
/// ```
///
/// Plain text first and HTML second, so clients show the richer part; the image sits next to
/// the HTML that uses it, so clients don't list it as an attachment.
///
/// Header values that come from the user's config or from email pass through
/// [`clean_line`], so none of them can contain a line break and start a new header.
pub fn compose(rendered: &Rendered, envelope: &Envelope<'_>) -> io::Result<Vec<u8>> {
    let text = MimePart::new(
        "text/plain",
        BodyPart::Text(Cow::from(rendered.text.as_str())),
    );
    let html = MimePart::new(
        "text/html",
        BodyPart::Text(Cow::from(rendered.html.as_str())),
    );
    let image = MimePart::new("image/jpeg", BodyPart::Binary(Cow::Borrowed(HEADER_IMAGE)))
        .inline()
        .cid(HEADER_CID);
    let body = MimePart::new(
        "multipart/alternative",
        vec![text, MimePart::new("multipart/related", vec![html, image])],
    );

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
        .body(body)
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
            colour: flag.colour(),
            title: title.into(),
            items,
            hidden: 0,
        }
    }

    /// A digest with every kind of content.
    fn full_digest() -> Digest {
        let mut due_item = item("billing@power.example", "Invoice 77", "An unpaid invoice.");
        due_item.deadline = Some(civil::date(2026, 11, 30));
        due_item.received = civil::date(2026, 10, 26).at(6, 12, 0, 0);
        Digest {
            account: "work".into(),
            date: civil::date(2026, 10, 26),
            from: civil::date(2026, 10, 25).at(7, 0, 0, 0),
            to: civil::date(2026, 10, 26).at(7, 0, 0, 0),
            dry_run: false,
            classify_only: false,
            total: 57,
            sections: vec![
                section(
                    TriageFlag::ReplyNeeded,
                    "Reply needed",
                    vec![item(
                        "Alice Example",
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
            misfiled: vec![],
            misfiled_hidden: 0,
        }
    }

    #[test]
    fn the_text_part_of_a_full_digest() {
        let expected = "\
DAILY TRIAGE: work
Mon 26 Oct 2026
25 Oct 07:00 to 26 Oct 07:00, 57 messages

5 messages need you.

REPLY NEEDED (1)
- Alice Example, yesterday 14:02
  Contract draft
  Asks you to confirm the date.

DEADLINE (4)
- billing@power.example, 06:12, due Mon 30 Nov
  Invoice 77
  An unpaid invoice.
  and 3 more, flagged in Mail

THE REST
Bulk mail: 12 (newsletters, marketing, cold outreach)
Junk: 5 (spam and phishing)
Other mail: 5 (nothing to do)
Noisiest senders: promo@shop.example 5, news@paper.example 3

--
Written by Let's Triage Mail on your own machine. Nothing left it.
";
        assert_eq!(render(&full_digest()).text, expected);
    }

    #[test]
    fn times_are_relative_to_the_digest_date() {
        let today = civil::date(2026, 10, 27);
        assert_eq!(
            when(civil::date(2026, 10, 27).at(6, 12, 0, 0), today),
            "06:12"
        );
        assert_eq!(
            when(civil::date(2026, 10, 26).at(21, 3, 0, 0), today),
            "yesterday 21:03"
        );
        assert_eq!(
            when(civil::date(2026, 10, 24).at(11, 20, 0, 0), today),
            "sat 11:20"
        );
    }

    #[test]
    fn initials_come_from_a_name_or_the_address() {
        assert_eq!(initials("Anna Kowalska"), "AK");
        assert_eq!(initials("\"Дмитрий Смирнов\""), "ДС");
        assert_eq!(initials("billing@power.example"), "B");
        assert_eq!(initials(""), "?");
        assert_eq!(initials("123 456"), "?");
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
        let rendered = render(&digest);
        assert!(
            rendered.text.contains("Nothing needs you."),
            "{}",
            rendered.text
        );
        assert!(
            rendered.text.contains("Bulk mail: 2 ("),
            "{}",
            rendered.text
        );
        assert!(rendered.html.contains("nothing needs you"));
        assert!(
            rendered
                .html
                .contains("Nothing needs you. 2 messages sorted.")
        );

        digest.noise = Noise::default();
        digest.total = 0;
        let rendered = render(&digest);
        assert!(rendered.text.contains("No new mail."), "{}", rendered.text);
        assert!(rendered.text.contains(EMPTY_DAY));
        assert!(!rendered.text.contains("THE REST"), "{}", rendered.text);
        assert!(!rendered.html.contains("the rest"));
    }

    #[test]
    fn a_dry_run_digest_says_the_flags_were_not_set() {
        let mut digest = full_digest();
        digest.dry_run = true;
        let rendered = render(&digest);
        assert!(
            rendered
                .text
                .contains("DRY RUN: Nothing was flagged in Mail.")
        );
        assert!(
            rendered
                .text
                .contains("5 messages would have been flagged.")
        );
        assert!(rendered.html.contains("Nothing was flagged in Mail."));
        // The rest is not in Mail's Flagged mailbox, so the hidden line must not say so.
        assert!(
            rendered.text.contains("  and 3 more\n"),
            "{}",
            rendered.text
        );
    }

    #[test]
    fn a_classify_only_digest_says_it_is_the_only_output() {
        let digest = Digest {
            classify_only: true,
            ..full_digest()
        };
        let rendered = render(&digest);
        assert!(
            rendered
                .text
                .contains("FLAGS OFF: This account is sorted but never flagged")
        );
        assert!(rendered.html.contains("flags off"));
        assert!(!rendered.text.contains("flagged in Mail"));
    }

    #[test]
    fn the_html_follows_the_design() {
        let html = render(&full_digest()).html;
        // The lockup, the headline, a tile per flag with its colour, the period, lowercase.
        assert!(html.contains("triage mail</div>"));
        assert!(html.contains("5 messages need you"));
        assert!(html.contains("work &middot; mon 26 oct 2026"));
        assert!(html.contains("25 oct 07:00 – 26 oct 07:00 · 57 messages"));
        assert!(html.contains("class=\"d-orange\""));
        assert!(html.contains("class=\"d-yellow\""));
        assert!(html.contains("due mon 30 nov"));
        assert!(html.contains("and 3 more, flagged in Mail"));
        assert!(html.contains(">AE</div>"));
        // The preheader lists the flags.
        assert!(html.contains("1 reply needed, 4 deadline"));
    }

    #[test]
    fn the_html_has_no_remote_content_and_supports_dark_mode() {
        let html = render(&full_digest()).html;
        for remote in ["http://", "https://", "<img", "<link", "<script", "@import"] {
            assert!(!html.contains(remote), "found {remote} in the HTML");
        }
        // The one image is the header, carried inside the message.
        assert_eq!(html.matches("url(").count(), 1);
        assert!(html.contains("url(cid:digest-header)"));
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
        assert!(
            message
                .body_html(0)
                .unwrap()
                .contains("url(cid:digest-header)")
        );
        let raw = String::from_utf8_lossy(&bytes);
        assert!(raw.contains("multipart/alternative"), "{raw}");
        assert!(raw.contains("multipart/related"), "{raw}");
    }

    #[test]
    fn the_header_image_travels_inside_the_message() {
        let bytes = compose(&render(&full_digest()), &envelope()).unwrap();
        let message = mail_parser::MessageParser::default().parse(&bytes).unwrap();
        let image = message
            .parts()
            .find(|part| part.content_id() == Some(HEADER_CID))
            .expect("an inline part with the header's Content-ID");
        assert!(image.is_content_type("image", "jpeg"));
        assert!(image.is_inline());
        assert!(
            HEADER_IMAGE.len() < 200 * 1024,
            "the design caps it at 200 KB"
        );
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

    #[test]
    fn misfiled_mail_gets_its_own_section_before_the_rest() {
        let digest = Digest {
            misfiled: vec![item("anna@friend.example", "Lunch?", "")],
            misfiled_hidden: 2,
            ..full_digest()
        };
        let text = render(&digest).text;
        let expected = "\n\
POSSIBLY MISFILED IN SPAM (3)
People you know, with a valid signature, whose mail is in your Spam folder. Nothing was moved.
- anna@friend.example, yesterday 14:02
  Lunch?
  and 2 more
";
        assert!(text.contains(expected), "{text}");
        let section = text.find("POSSIBLY MISFILED").unwrap();
        assert!(section > text.find("DEADLINE (4)").unwrap(), "{text}");
        assert!(section < text.find("THE REST").unwrap(), "{text}");
    }

    #[test]
    fn no_misfiled_section_without_misfiled_mail() {
        let rendered = render(&full_digest());
        assert!(!rendered.text.to_lowercase().contains("misfiled"));
        assert!(!rendered.html.contains("misfiled"));
    }

    #[test]
    fn misfiled_html_is_escaped() {
        let digest = Digest {
            misfiled: vec![item(
                "<b>x</b>@evil.example",
                "<script>alert(1)</script>",
                "",
            )],
            ..full_digest()
        };
        let html = render(&digest).html;
        assert!(html.contains("possibly misfiled in spam</span>"), "{html}");
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;@evil.example"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<b>x</b>"), "{html}");
    }
}
