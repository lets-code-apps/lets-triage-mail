//! Integration tests for `message.rs`, driven by synthetic `.eml` fixtures.

use lets_triage_mail::decider::{AuthResult, Evidence};
use lets_triage_mail::engine::Category;
use lets_triage_mail::message::{
    AuthVerdict, ParseError, ProviderHeaders, html_to_text, parse_auth_results, parse_headers_only,
    parse_message,
};
use lets_triage_mail::precheck::precheck;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("tests/fixtures/{name}")).expect("fixture exists")
}

#[test]
fn newsletter_headers_and_bulk_signals() {
    let m = parse_message(&fixture("newsletter.eml"), None, None).unwrap();
    assert_eq!(m.from_address, "news@shop.example");
    assert_eq!(m.from_name, "Shop");
    assert_eq!(m.from_domain(), "shop.example");
    assert_eq!(m.subject, "Weekly digest");
    assert_eq!(m.message_id.as_deref(), Some("weekly-1@shop.example"));
    assert_eq!(m.in_reply_to, ["prev-1@shop.example"]);
    assert_eq!(m.references, ["root-1@shop.example", "prev-1@shop.example"]);
    assert_eq!(m.to, ["demouser@example.com"]);
    assert_eq!(m.cc, ["friend@example.org"]);
    assert!(
        m.date
            .as_deref()
            .unwrap()
            .starts_with("2026-10-24T12:00:00")
    );
    assert!(m.is_bulk());
    assert!(m.has_list_unsubscribe);
    assert_eq!(m.list_id.as_deref(), Some("<weekly.shop.example>"));
    assert_eq!(m.precedence.as_deref(), Some("bulk"));
    assert!(!m.is_auto_submitted);
    assert!(m.auth.all_pass());
    assert_eq!(m.auth.dkim_domains, ["shop.example"]);
    assert_eq!(m.auth.summary(), "spf=pass dkim=pass dmarc=pass");
    assert!(m.size > 0);
    assert!(!m.headers_only);
}

#[test]
fn html_body_is_flattened_and_scripts_dropped() {
    let m = parse_message(&fixture("newsletter.eml"), None, None).unwrap();
    assert!(m.body_text.contains("Hello Piotr"), "{}", m.body_text);
    assert!(!m.body_text.contains("evil"));
    assert!(!m.body_text.contains("color:red"));
    assert!(!m.body_text.contains("Hidden title"));
    assert_eq!(m.link_count, 2);
}

#[test]
fn spoof_signals() {
    let m = parse_message(&fixture("spoof.eml"), None, None).unwrap();
    assert!(m.display_name_spoof());
    assert!(m.reply_to_mismatch());
    assert!(m.auth.any_fail());
    assert_eq!(m.auth.dkim, AuthVerdict::None);

    let friend = parse_message(&fixture("friend.eml"), None, None).unwrap();
    assert!(!friend.display_name_spoof());
    assert!(!friend.reply_to_mismatch());
    assert!(!friend.is_bulk());
    assert!(
        !friend.is_auto_submitted,
        "Auto-Submitted: no is not automatic"
    );
}

#[test]
fn headers_only_skips_the_body_but_decodes_the_subject() {
    let m = parse_headers_only(&fixture("encoded_subject.eml"), None, None).unwrap();
    assert_eq!(m.subject, "Zamówienie gotowe");
    assert!(m.body_text.is_empty());
    assert!(m.headers_only);

    let full = parse_message(&fixture("encoded_subject.eml"), None, None).unwrap();
    assert!(
        full.body_text.starts_with("Body text"),
        "{:?}",
        full.body_text
    );
}

#[test]
fn topmost_authentication_results_wins_without_a_configured_id() {
    let m = parse_message(&fixture("forged_auth.eml"), None, None).unwrap();
    assert_eq!(m.auth.dmarc, AuthVerdict::Fail);
    assert_eq!(m.auth.dkim, AuthVerdict::Fail);
}

#[test]
fn configured_authserv_id_ignores_other_servers() {
    let m = parse_message(&fixture("forged_auth.eml"), Some("MX.mine.example"), None).unwrap();
    assert_eq!(m.auth.spf, AuthVerdict::Fail);

    // No header from our server at all: nothing is trusted.
    let m = parse_message(&fixture("forged_auth.eml"), Some("other.example"), None).unwrap();
    assert_eq!(m.auth.summary(), "spf=none dkim=none dmarc=none");
}

#[test]
fn forged_header_is_ignored_even_when_it_comes_first() {
    let forged = "evil.example; dkim=pass header.d=bank.example; dmarc=pass".to_string();
    let real = "mx.mine.example; spf=fail; dkim=fail; dmarc=fail".to_string();
    let r = parse_auth_results(&[forged, real], Some("mx.mine.example"));
    assert_eq!(r.dkim, AuthVerdict::Fail);
    assert!(r.any_fail());
}

#[test]
fn attachments_and_plain_text_links() {
    let m = parse_message(&fixture("attachment.eml"), None, None).unwrap();
    assert!(m.has_attachments);
    assert_eq!(m.attachment_names, ["invoice.pdf"]);
    assert!(m.body_text.contains("pay by Friday"));
    assert!(
        !m.body_text.contains("JVBER"),
        "attachment must not leak into the body"
    );
    assert_eq!(m.link_count, 1);
}

#[test]
fn garbage_input_does_not_panic() {
    // No header block at all.
    assert_eq!(parse_message(b"", None, None), Err(ParseError::Unparseable));
    // Binary noise: either an error or a parsed message, never a panic.
    let noise: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let _ = parse_message(&noise, None, None);
    let _ = parse_headers_only(&noise, None, None);
}

#[test]
fn several_dkim_signatures_one_pass_is_enough() {
    let header =
        "mx.t; dkim=fail header.d=esp.example; dkim=pass header.d=Shop.example; dmarc=pass";
    let r = parse_auth_results(&[header.to_string()], None);
    assert_eq!(r.dkim, AuthVerdict::Pass);
    assert_eq!(r.dkim_domains, ["shop.example"]);

    // Order must not matter.
    let header = "mx.t; dkim=pass header.d=shop.example; dkim=fail header.d=esp.example";
    assert_eq!(
        parse_auth_results(&[header.to_string()], None).dkim,
        AuthVerdict::Pass
    );
}

#[test]
fn html_to_text_survives_garbage() {
    let (text, links) = html_to_text("<div><p>unclosed <a href='https://x'>link");
    assert!(text.contains("unclosed"));
    assert_eq!(links, 1);

    // Unterminated script and tags, stray angle brackets.
    let (text, _) = html_to_text("<<<>>> <script>never closed <b>visible?</b>");
    assert!(!text.contains("visible"));
    let _ = html_to_text("\u{0}<a href=<\"><svg><math></p></table></td><!-- unterminated");

    // Nesting deep enough to overflow a recursive walker.
    // Hostile nesting is refused quickly instead of stalling the parser (it is quadratic).
    let started = std::time::Instant::now();
    let (text, _) = html_to_text(&"<div>".repeat(100_000));
    assert!(text.is_empty());
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    // Plausible nesting is fine.
    let nested = format!("{}deep text{}", "<div>".repeat(50), "</div>".repeat(50));
    assert!(html_to_text(&nested).0.contains("deep text"));
}

#[test]
fn debug_output_hides_the_body() {
    let raw = b"From: a@example.com\r\nSubject: hi\r\n\r\nsecret body text\r\n";
    let msg = lets_triage_mail::message::parse_message(raw, None, None).unwrap();
    let debug = format!("{msg:?}");
    assert!(!debug.contains("secret body text"));
    assert!(debug.contains("hidden"));
}

// ---- task 1.5: sender signals, provider headers and the pre-check ----------------------------

#[test]
fn rotating_address_casino_spam_is_certain_spam_without_the_model() {
    let raw = fixture("casino_rotating.eml");
    let m = parse_message(&raw, None, Some(ProviderHeaders::Wp)).unwrap();
    // `mail-parser` keeps the broken address as written; we judge it.
    assert_eq!(
        m.from_address,
        "me.name.2lkxp6bb.fnvrb@volny.example..sn.live.example"
    );
    assert!(m.malformed_sender());
    // The recipient's own local part, then a random tail, on another domain.
    assert!(m.impersonates_recipient());
    assert_eq!(m.provider_spam, Some(true));
    // "no signature" is no DKIM result.
    assert_eq!(m.auth.dkim, AuthVerdict::None);

    let evidence = Evidence::from_message(&m, false);
    assert!(evidence.malformed_sender);
    let verdict = precheck(&evidence).expect("a rule verdict");
    assert_eq!(verdict.category, Category::Spam);
    assert_eq!(verdict.confidence, 1.0);
    assert_eq!(verdict.reason, "invalid sender address");
}

#[test]
fn the_casino_rule_does_not_depend_on_provider_headers() {
    let m = parse_message(&fixture("casino_rotating.eml"), None, None).unwrap();
    assert_eq!(m.provider_spam, None);
    let evidence = Evidence::from_message(&m, false);
    assert!(precheck(&evidence).is_some());
}

#[test]
fn agency_marketing_gets_provider_facts_but_no_rule_verdict() {
    let raw = fixture("agency_marketing.eml");
    let m = parse_message(&raw, None, Some(ProviderHeaders::Wp)).unwrap();
    assert_eq!(m.auth.dkim, AuthVerdict::Pass);
    // The provider calls it spam; that is a hint for the model, not a verdict.
    assert_eq!(m.provider_spam, Some(true));
    assert!(m.has_list_unsubscribe);
    assert!(m.reply_to_mismatch());
    assert!(!m.malformed_sender());
    assert!(!m.impersonates_recipient());

    let evidence = Evidence::from_message(&m, false);
    assert_eq!(evidence.dkim, AuthResult::Pass);
    assert_eq!(evidence.provider_spam, Some(true));
    assert_eq!(precheck(&evidence), None);
}

#[test]
fn agency_marketing_without_the_opt_in_ignores_the_provider_headers() {
    let m = parse_message(&fixture("agency_marketing.eml"), None, None).unwrap();
    assert_eq!(m.auth.dkim, AuthVerdict::None);
    assert_eq!(m.provider_spam, None);
}

#[test]
fn ordinary_fixtures_trigger_no_sender_signal() {
    for name in [
        "friend.eml",
        "newsletter.eml",
        "attachment.eml",
        "spoof.eml",
    ] {
        let m = parse_message(&fixture(name), None, Some(ProviderHeaders::Wp)).unwrap();
        assert!(!m.malformed_sender(), "{name}");
        assert!(!m.impersonates_recipient(), "{name}");
    }
}
