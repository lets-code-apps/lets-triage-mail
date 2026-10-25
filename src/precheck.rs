//! Deterministic rules that decide before the model is asked.
//!
//! Some spam is certain from facts our own code can check, and asking a thinking model about
//! it costs about 12 seconds per message. [`precheck`] is pure (no IO, no clock), so
//! `classify` and the account worker share it and both behave the same.
//!
//! It only ever says "spam, certain". It can never make a message look *better*, and it
//! never acts on the mailbox: the verdict goes through the same `choose_flag` as the
//! model's, which never flags spam.

use crate::decider::{AuthResult, Evidence};
use crate::engine::{Category, Payment, Verdict};

/// The value stored in `processed.source` (and shown by `classify`) for a rule's verdict.
pub const SOURCE_RULE: &str = "rule";
/// The value stored in `processed.source` for the model's verdict.
pub const SOURCE_MODEL: &str = "model";

/// A spam verdict when the evidence is conclusive, otherwise `None` and the model decides.
///
/// - The sender address is malformed: no real mail server sends from such an address.
/// - A stranger impersonates the recipient and DKIM does not pass. With a DKIM pass the
///   signing domain vouches for the message, so it stays with the model.
///
/// The provider's own spam verdict (`provider_spam`) is deliberately *not* a rule: providers
/// also mark legitimate bulk mail as spam. It is evidence for the model only.
pub fn precheck(evidence: &Evidence) -> Option<Verdict> {
    let reason = if evidence.malformed_sender {
        "invalid sender address"
    } else if evidence.impersonates_recipient && evidence.dkim != AuthResult::Pass {
        "sender impersonates the recipient and has no valid signature"
    } else {
        // `return` inside the `else` branch: this arm has no value, the function ends here.
        return None;
    };
    Some(Verdict {
        category: Category::Spam,
        needs_reply: false,
        deadline: None,
        payment: Payment::NotApplicable,
        confidence: 1.0,
        reason: reason.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unremarkable message: no signal set.
    fn plain_evidence() -> Evidence {
        Evidence {
            from_address: "anna@friend.example".into(),
            from_name: "Anna".into(),
            reply_to: None,
            subject: "Lunch?".into(),
            date: None,
            body_text: "Are you free on Friday?".into(),
            headers_only: false,
            attachment_names: vec![],
            spf: AuthResult::Unknown,
            dkim: AuthResult::Unknown,
            dmarc: AuthResult::Unknown,
            provider_spam: None,
            bulk_headers: false,
            auto_submitted: false,
            known_domain: false,
            reply_to_mismatch: false,
            display_name_spoof: false,
            malformed_sender: false,
            impersonates_recipient: false,
            link_count: 0,
        }
    }

    #[test]
    fn ordinary_mail_is_left_to_the_model() {
        assert_eq!(precheck(&plain_evidence()), None);
    }

    #[test]
    fn a_malformed_sender_is_certain_spam() {
        let evidence = Evidence {
            malformed_sender: true,
            ..plain_evidence()
        };
        let verdict = precheck(&evidence).expect("a verdict");
        assert_eq!(verdict.category, Category::Spam);
        assert_eq!(verdict.confidence, 1.0);
        assert!(!verdict.needs_reply);
        assert_eq!(verdict.reason, "invalid sender address");
    }

    #[test]
    fn a_malformed_sender_is_spam_even_with_a_dkim_pass() {
        let evidence = Evidence {
            malformed_sender: true,
            dkim: AuthResult::Pass,
            ..plain_evidence()
        };
        assert!(precheck(&evidence).is_some());
    }

    #[test]
    fn impersonation_needs_a_missing_dkim_pass() {
        for (dkim, fires) in [
            (AuthResult::Unknown, true),
            (AuthResult::Fail, true),
            (AuthResult::Pass, false),
        ] {
            let evidence = Evidence {
                impersonates_recipient: true,
                dkim,
                ..plain_evidence()
            };
            assert_eq!(precheck(&evidence).is_some(), fires, "{dkim:?}");
        }
    }

    #[test]
    fn the_provider_spam_verdict_alone_never_decides() {
        let evidence = Evidence {
            provider_spam: Some(true),
            ..plain_evidence()
        };
        assert_eq!(precheck(&evidence), None);
    }
}
