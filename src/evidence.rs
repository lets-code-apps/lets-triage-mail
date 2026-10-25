//! Glue between message parsing and the decider: [`ParsedMessage`] in, [`Evidence`] out.
//!
//! The decider deliberately knows nothing about the parser (see `decider/mod.rs`), so the
//! conversion lives here, in the one place that knows both.
//!
//! The two modules also have their own authentication enums, and we keep both on purpose.
//! The parser's [`AuthVerdict`] keeps every keyword a server may write (`softfail`,
//! `temperror`, ...) because the engine and logs may care. The decider's [`AuthResult`] is
//! the small vocabulary the prompt and the guard rail use: pass, fail, unknown. Collapsing
//! the parser's type into three states would lose information; exposing seven states to the
//! prompt would only add noise. A `From` impl maps one onto the other.

use crate::decider::{AuthResult, Evidence};
use crate::message::{AuthVerdict, ParsedMessage};

impl From<AuthVerdict> for AuthResult {
    /// Only a clear pass or fail is kept. `softfail`, `neutral`, `none`, errors and unknown
    /// keywords are all "we cannot say", which the prompt calls `unknown`.
    fn from(verdict: AuthVerdict) -> AuthResult {
        match verdict {
            AuthVerdict::Pass => AuthResult::Pass,
            AuthVerdict::Fail => AuthResult::Fail,
            _ => AuthResult::Unknown,
        }
    }
}

impl Evidence {
    /// Builds the evidence for one parsed message.
    ///
    /// `known_domain` is a parameter because it comes from the database (has the user ever
    /// written to this domain?), which the caller owns. `classify FILE` has no database and
    /// passes `false`.
    pub fn from_message(message: &ParsedMessage, known_domain: bool) -> Evidence {
        Evidence {
            from_address: message.from_address.clone(),
            from_name: message.from_name.clone(),
            reply_to: message.reply_to.clone(),
            subject: message.subject.clone(),
            date: message.date.clone(),
            body_text: message.body_text.clone(),
            headers_only: message.headers_only,
            attachment_names: message.attachment_names.clone(),
            spf: message.auth.spf.into(),
            dkim: message.auth.dkim.into(),
            dmarc: message.auth.dmarc.into(),
            provider_spam: message.provider_spam,
            malformed_sender: message.malformed_sender(),
            impersonates_recipient: message.impersonates_recipient(),
            bulk_headers: message.is_bulk(),
            auto_submitted: message.is_auto_submitted,
            known_domain,
            reply_to_mismatch: message.reply_to_mismatch(),
            display_name_spoof: message.display_name_spoof(),
            link_count: message.link_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::parse_message;

    #[test]
    fn only_pass_and_fail_survive_the_mapping() {
        assert_eq!(AuthResult::from(AuthVerdict::Pass), AuthResult::Pass);
        assert_eq!(AuthResult::from(AuthVerdict::Fail), AuthResult::Fail);
        for other in [
            AuthVerdict::SoftFail,
            AuthVerdict::Neutral,
            AuthVerdict::None,
            AuthVerdict::Error,
            AuthVerdict::Other,
        ] {
            assert_eq!(AuthResult::from(other), AuthResult::Unknown, "{other:?}");
        }
    }

    #[test]
    fn evidence_carries_the_message_fields() {
        let raw = include_bytes!("../tests/fixtures/friend.eml");
        let message = parse_message(raw, None, None).unwrap();
        let evidence = Evidence::from_message(&message, true);
        assert_eq!(evidence.from_address, "anna@friend.example");
        assert_eq!(evidence.subject, "Lunch?");
        assert!(evidence.body_text.contains("free on Friday"));
        assert!(evidence.known_domain);
        assert!(!evidence.bulk_headers);
        assert!(!evidence.headers_only);
        assert_eq!(evidence.spf, AuthResult::Unknown);
    }

    #[test]
    fn evidence_maps_newsletter_signals_and_auth() {
        let raw = include_bytes!("../tests/fixtures/newsletter.eml");
        let message = parse_message(raw, None, None).unwrap();
        let evidence = Evidence::from_message(&message, false);
        assert!(evidence.bulk_headers);
        assert!(!evidence.known_domain);
        assert_eq!(evidence.link_count, message.link_count);
    }

    #[test]
    fn failed_auth_and_spoofing_are_mapped() {
        let raw = include_bytes!("../tests/fixtures/spoof.eml");
        let message = parse_message(raw, None, None).unwrap();
        let evidence = Evidence::from_message(&message, false);
        assert_eq!(evidence.display_name_spoof, message.display_name_spoof());
        assert_eq!(evidence.reply_to_mismatch, message.reply_to_mismatch());
        assert_eq!(evidence.dmarc, message.auth.dmarc.into());
    }
}
