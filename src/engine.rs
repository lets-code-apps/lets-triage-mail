//! The decision engine: from what we know about a message to the flag it should get.
//!
//! This module does no IO. The caller looks up rules and known senders in the database,
//! asks the model for a [`Verdict`], and passes the results in. The pipeline is:
//!
//! 1. [`screen_sender`]: allow/block rules and known senders decide how far to trust the sender.
//!    A blocked sender stops here, so the model is never called for it.
//! 2. The caller asks the model for a [`Verdict`].
//! 3. [`choose_flag`]: the verdict, the trust level and the user's [`FlagPolicy`] pick at most
//!    one [`TriageFlag`].

use crate::flags::FlagColour;

/// What kind of message this is, as judged by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Personal,
    Transactional,
    Newsletter,
    Marketing,
    ColdOutreach,
    Spam,
    Phishing,
}

impl Category {
    /// The lowercase name used in the prompt, in `classify --json` and in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Personal => "personal",
            Category::Transactional => "transactional",
            Category::Newsletter => "newsletter",
            Category::Marketing => "marketing",
            Category::ColdOutreach => "cold_outreach",
            Category::Spam => "spam",
            Category::Phishing => "phishing",
        }
    }

    /// Whether a message from an unknown sender in this category may get an action flag.
    ///
    /// Spam and phishing must never be flagged: a phishing "invoice" flagged "To pay" would
    /// help the attacker. Newsletters, marketing and cold outreach are noise, not actions.
    pub fn can_be_flagged(self) -> bool {
        matches!(self, Category::Personal | Category::Transactional)
    }
}

/// What the message says about money.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payment {
    /// An invoice, bill or payment request that is still open.
    Unpaid,
    /// A receipt or paid invoice.
    Paid,
    NotApplicable,
}

impl Payment {
    /// The lowercase name used in the prompt, in `classify --json` and in logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Payment::Unpaid => "unpaid",
            Payment::Paid => "paid",
            Payment::NotApplicable => "not_applicable",
        }
    }
}

/// The model's judgement of one message.
// No `Eq` here: `f32` is only `PartialEq`, because NaN is not equal to itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub category: Category,
    pub needs_reply: bool,
    /// The date to act by. A calendar date without a time zone (`jiff::civil::Date`): the
    /// model reads "by 30 November" from the mail, and "has it passed?" is answered against
    /// the user's local today. Anything the model wrote that is not a real date is rejected
    /// when the verdict is parsed, so a `Some` here is always valid.
    pub deadline: Option<jiff::civil::Date>,
    pub payment: Payment,
    /// How sure the model is, from 0.0 to 1.0.
    pub confidence: f32,
    /// One sentence explaining the verdict, shown in the digest.
    pub reason: String,
}

/// A user-defined rule for a sender address or `@domain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    Allow,
    Block,
}

/// How far we trust a message's sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// On the user's block list: never flag, don't ask the model.
    Blocked,
    /// On the allow list, or someone the user has written to.
    Trusted,
    Unknown,
}

/// Decides how far to trust the sender, before the model is asked anything.
///
/// A message that fails SPF/DKIM/DMARC is never trusted, even from a known or allowed
/// address: the `From` header may be forged.
pub fn screen_sender(rule: Option<Rule>, known_sender: bool, auth_failed: bool) -> Trust {
    match rule {
        Some(Rule::Block) => Trust::Blocked,
        _ if auth_failed => Trust::Unknown,
        Some(Rule::Allow) => Trust::Trusted,
        None if known_sender => Trust::Trusted,
        None => Trust::Unknown,
    }
}

/// The flags the service can set, each meaning "this needs an action from you".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageFlag {
    ReplyNeeded,
    Deadline,
    ToPay,
    Bookkeeping,
    Unsure,
}

impl TriageFlag {
    /// The default colour from `docs/FLAGS.md`. None of them is red: red is reserved for
    /// the user.
    pub fn colour(self) -> FlagColour {
        match self {
            TriageFlag::ReplyNeeded => FlagColour::Orange,
            TriageFlag::Deadline => FlagColour::Yellow,
            TriageFlag::ToPay => FlagColour::Green,
            TriageFlag::Bookkeeping => FlagColour::Purple,
            TriageFlag::Unsure => FlagColour::Gray,
        }
    }
}

/// The colour each [`TriageFlag`] is shown in. Configurable, so other users can pick their own
/// meanings; [`TriageFlag::colour`] gives the defaults. The config loader rejects red.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlagColours {
    pub reply_needed: FlagColour,
    pub deadline: FlagColour,
    pub to_pay: FlagColour,
    pub bookkeeping: FlagColour,
    pub unsure: FlagColour,
}

impl Default for FlagColours {
    fn default() -> Self {
        FlagColours {
            reply_needed: TriageFlag::ReplyNeeded.colour(),
            deadline: TriageFlag::Deadline.colour(),
            to_pay: TriageFlag::ToPay.colour(),
            bookkeeping: TriageFlag::Bookkeeping.colour(),
            unsure: TriageFlag::Unsure.colour(),
        }
    }
}

/// Which flags the user has enabled, and how sure the model must be to act.
#[derive(Debug, Clone, PartialEq)]
pub struct FlagPolicy {
    pub reply_needed: bool,
    pub deadline: bool,
    pub to_pay: bool,
    pub bookkeeping: bool,
    pub unsure: bool,
    /// The colour of each flag.
    pub colours: FlagColours,
    /// Below this confidence the message gets "Unsure" instead of an action flag.
    pub min_confidence: f32,
}

/// The defaults from `docs/FLAGS.md`: only "Reply needed" and "Unsure", to keep flag noise low.
impl Default for FlagPolicy {
    fn default() -> Self {
        FlagPolicy {
            reply_needed: true,
            deadline: false,
            to_pay: false,
            bookkeeping: false,
            unsure: true,
            colours: FlagColours::default(),
            min_confidence: 0.75,
        }
    }
}

impl FlagPolicy {
    /// The colour configured for a flag. Use this instead of [`TriageFlag::colour`] when
    /// setting flags on the server, so the user's config is respected.
    pub fn colour_for(&self, flag: TriageFlag) -> FlagColour {
        match flag {
            TriageFlag::ReplyNeeded => self.colours.reply_needed,
            TriageFlag::Deadline => self.colours.deadline,
            TriageFlag::ToPay => self.colours.to_pay,
            TriageFlag::Bookkeeping => self.colours.bookkeeping,
            TriageFlag::Unsure => self.colours.unsure,
        }
    }

    fn is_enabled(&self, flag: TriageFlag) -> bool {
        match flag {
            TriageFlag::ReplyNeeded => self.reply_needed,
            TriageFlag::Deadline => self.deadline,
            TriageFlag::ToPay => self.to_pay,
            TriageFlag::Bookkeeping => self.bookkeeping,
            TriageFlag::Unsure => self.unsure,
        }
    }
}

/// Picks at most one flag for a message.
///
/// Priority: Reply needed > Deadline > To pay > Bookkeeping. A disabled flag is skipped, so
/// the next one that applies wins. If the model is not confident enough, the message gets
/// "Unsure" (when enabled) instead of an action flag.
pub fn choose_flag(trust: Trust, verdict: &Verdict, policy: &FlagPolicy) -> Option<TriageFlag> {
    if trust == Trust::Blocked {
        return None;
    }

    // Written as "confident" rather than "confidence < min" so that a NaN confidence counts
    // as not confident: every comparison with NaN is false.
    let confident = verdict.confidence >= policy.min_confidence;
    if !confident {
        return Some(TriageFlag::Unsure).filter(|&flag| policy.is_enabled(flag));
    }

    if trust == Trust::Unknown && !verdict.category.can_be_flagged() {
        return None;
    }

    // Highest priority first.
    let candidates = [
        (TriageFlag::ReplyNeeded, verdict.needs_reply),
        (TriageFlag::Deadline, verdict.deadline.is_some()),
        (TriageFlag::ToPay, verdict.payment == Payment::Unpaid),
        (TriageFlag::Bookkeeping, verdict.payment == Payment::Paid),
    ];
    candidates
        .into_iter()
        .find(|&(flag, applies)| applies && policy.is_enabled(flag))
        .map(|(flag, _)| flag)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A confident verdict that asks for nothing. Tests change only the fields they care about.
    fn verdict(category: Category) -> Verdict {
        Verdict {
            category,
            needs_reply: false,
            deadline: None,
            payment: Payment::NotApplicable,
            confidence: 0.9,
            reason: String::from("test"),
        }
    }

    fn all_flags_enabled() -> FlagPolicy {
        FlagPolicy {
            deadline: true,
            to_pay: true,
            bookkeeping: true,
            ..FlagPolicy::default()
        }
    }

    #[test]
    fn block_rule_wins_over_everything() {
        assert_eq!(
            screen_sender(Some(Rule::Block), true, false),
            Trust::Blocked
        );
    }

    #[test]
    fn known_sender_is_trusted() {
        assert_eq!(screen_sender(None, true, false), Trust::Trusted);
        assert_eq!(screen_sender(None, false, false), Trust::Unknown);
    }

    #[test]
    fn failed_authentication_cancels_trust() {
        assert_eq!(screen_sender(None, true, true), Trust::Unknown);
        assert_eq!(
            screen_sender(Some(Rule::Allow), false, true),
            Trust::Unknown
        );
    }

    #[test]
    fn personal_mail_that_needs_a_reply_is_flagged() {
        let v = Verdict {
            needs_reply: true,
            ..verdict(Category::Personal)
        };
        let flag = choose_flag(Trust::Unknown, &v, &FlagPolicy::default());
        assert_eq!(flag, Some(TriageFlag::ReplyNeeded));
    }

    #[test]
    fn reply_needed_beats_to_pay() {
        let v = Verdict {
            needs_reply: true,
            payment: Payment::Unpaid,
            ..verdict(Category::Transactional)
        };
        let flag = choose_flag(Trust::Unknown, &v, &all_flags_enabled());
        assert_eq!(flag, Some(TriageFlag::ReplyNeeded));
    }

    #[test]
    fn disabled_flag_falls_through_to_the_next_one() {
        let v = Verdict {
            needs_reply: true,
            payment: Payment::Unpaid,
            ..verdict(Category::Transactional)
        };
        let policy = FlagPolicy {
            reply_needed: false,
            ..all_flags_enabled()
        };
        assert_eq!(
            choose_flag(Trust::Unknown, &v, &policy),
            Some(TriageFlag::ToPay)
        );
    }

    #[test]
    fn phishing_invoice_from_unknown_sender_is_never_flagged_to_pay() {
        let v = Verdict {
            payment: Payment::Unpaid,
            ..verdict(Category::Phishing)
        };
        assert_eq!(choose_flag(Trust::Unknown, &v, &all_flags_enabled()), None);
    }

    #[test]
    fn trusted_sender_is_flagged_whatever_the_category() {
        let v = Verdict {
            needs_reply: true,
            ..verdict(Category::Marketing)
        };
        let flag = choose_flag(Trust::Trusted, &v, &FlagPolicy::default());
        assert_eq!(flag, Some(TriageFlag::ReplyNeeded));
    }

    #[test]
    fn blocked_sender_is_never_flagged() {
        let v = Verdict {
            needs_reply: true,
            ..verdict(Category::Personal)
        };
        assert_eq!(
            choose_flag(Trust::Blocked, &v, &FlagPolicy::default()),
            None
        );
    }

    #[test]
    fn low_confidence_is_unsure() {
        let v = Verdict {
            needs_reply: true,
            confidence: 0.4,
            ..verdict(Category::Personal)
        };
        let flag = choose_flag(Trust::Unknown, &v, &FlagPolicy::default());
        assert_eq!(flag, Some(TriageFlag::Unsure));
    }

    #[test]
    fn nan_confidence_is_unsure() {
        let v = Verdict {
            needs_reply: true,
            confidence: f32::NAN,
            ..verdict(Category::Personal)
        };
        let flag = choose_flag(Trust::Unknown, &v, &FlagPolicy::default());
        assert_eq!(flag, Some(TriageFlag::Unsure));
    }

    #[test]
    fn nothing_to_do_means_no_flag() {
        let v = verdict(Category::Personal);
        assert_eq!(choose_flag(Trust::Trusted, &v, &all_flags_enabled()), None);
    }

    #[test]
    fn default_policy_uses_the_documented_colours() {
        let policy = FlagPolicy::default();
        assert_eq!(
            policy.colour_for(TriageFlag::ReplyNeeded),
            FlagColour::Orange
        );
        assert_eq!(policy.colour_for(TriageFlag::Unsure), FlagColour::Gray);
    }

    #[test]
    fn configured_colour_overrides_the_default() {
        let policy = FlagPolicy {
            colours: FlagColours {
                to_pay: FlagColour::Blue,
                ..FlagColours::default()
            },
            ..FlagPolicy::default()
        };
        assert_eq!(policy.colour_for(TriageFlag::ToPay), FlagColour::Blue);
    }

    #[test]
    fn no_triage_flag_is_red() {
        let all = [
            TriageFlag::ReplyNeeded,
            TriageFlag::Deadline,
            TriageFlag::ToPay,
            TriageFlag::Bookkeeping,
            TriageFlag::Unsure,
        ];
        assert!(all.iter().all(|flag| flag.colour() != FlagColour::Red));
    }
}
