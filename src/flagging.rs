//! The rules of the flag lifecycle, as pure functions. No IO.
//!
//! Setting and clearing flags changes someone's real mailbox, so every *decision* lives here as
//! a function from plain values to a plain answer, and is tested to death. The code that talks
//! to the server and the database (`imap/worker.rs`) only gathers the inputs and carries out
//! the answer. Four questions are answered:
//!
//! 1. [`plan_apply`]: may we set this colour on a message whose flags are these, and with which
//!    `STORE` change?
//! 2. [`owned_state`]: a flag we set earlier: is it still exactly what we set, or did the user
//!    touch it?
//! 3. [`clear_reason`]: has a flag we own served its purpose (the user replied, the deadline
//!    passed)?
//! 4. [`reply_ids_from_header`]: which messages did a sent message answer?
//!
//! The hard rules from `CLAUDE.md` show up here as code: red is never planned, a message that
//! is already flagged is never touched, and a flag that differs from what we set belongs to
//! the user from then on.

use jiff::civil::Date;

use crate::engine::TriageFlag;
use crate::flags::{FLAGGED, FlagColour, StoreChange};

// ---------------------------------------------------------------------------------------------
// Setting a flag
// ---------------------------------------------------------------------------------------------

/// Why a message is left alone instead of being flagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveReason {
    /// `\Flagged` is already there: the user (or another client) got there first.
    AlreadyFlagged,
    /// The colour is red, which is reserved for the user. The config loader rejects red, so
    /// this is a second line of defence, not a normal path.
    ReservedRed,
}

/// What to do about a message we would like to flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyPlan {
    /// Do not touch the message.
    Leave(LeaveReason),
    /// Send this change to the server.
    Store(StoreChange),
}

/// Decides whether `colour` may be set on a message whose **current** flags are
/// `current_flags` (read from the server right before the change, not at download time).
///
/// Any `\Flagged`, of any colour, means the user has already dealt with the message; the
/// service only ever sets a flag on a message that has none.
pub fn plan_apply(current_flags: &[String], colour: FlagColour) -> ApplyPlan {
    if has_flag(current_flags, FLAGGED) {
        return ApplyPlan::Leave(LeaveReason::AlreadyFlagged);
    }
    match colour.store_change() {
        Some(change) => ApplyPlan::Store(change),
        None => ApplyPlan::Leave(LeaveReason::ReservedRed),
    }
}

// ---------------------------------------------------------------------------------------------
// Flags we own
// ---------------------------------------------------------------------------------------------

/// Whether a flag we set is still as we left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnedState {
    /// The message still carries exactly the colour we set.
    Intact,
    /// Anything else: the user changed the colour, removed the flag, or flagged it red. From
    /// now on the flag is theirs and the service never touches the message again.
    ChangedByUser,
}

/// Compares a message's current flags with the colour we recorded.
///
/// "Exactly" is deliberate: a different colour, no `\Flagged` at all (the user unflagged it),
/// and bare `\Flagged` (red, the user's own colour) all count as the user's doing. Other flags
/// such as `\Seen` or `\Answered` are irrelevant.
pub fn owned_state(current_flags: &[String], recorded: FlagColour) -> OwnedState {
    let current: Vec<&str> = current_flags.iter().map(String::as_str).collect();
    if FlagColour::from_flags(&current) == Some(recorded) {
        OwnedState::Intact
    } else {
        OwnedState::ChangedByUser
    }
}

/// Why an owned flag should be removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearReason {
    /// "Reply needed", and the user's reply is in the Sent folder.
    Replied,
    /// "Deadline", and the day has passed.
    DeadlinePassed,
}

impl ClearReason {
    /// Short text for the logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ClearReason::Replied => "the reply is in Sent",
            ClearReason::DeadlinePassed => "the deadline has passed",
        }
    }
}

/// Should this owned flag be cleared now?
///
/// Only two flags clear themselves (see `docs/FLAGS.md`):
///
/// - **Reply needed** when `replied` is true.
/// - **Deadline** when `today` is after the date. The flag stays up *on* the deadline day,
///   which is still the last day to act.
///
/// Everything else (To pay, Bookkeeping, Unsure) waits for the user. A "Deadline" flag with
/// no readable date is never cleared: when in doubt the flag stays.
pub fn clear_reason(
    flag: Option<TriageFlag>,
    deadline: Option<Date>,
    replied: bool,
    today: Date,
) -> Option<ClearReason> {
    match flag? {
        TriageFlag::ReplyNeeded if replied => Some(ClearReason::Replied),
        TriageFlag::Deadline if deadline.is_some_and(|date| today > date) => {
            Some(ClearReason::DeadlinePassed)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Replies in Sent
// ---------------------------------------------------------------------------------------------

/// At most this many ids are kept from one sent message. A long thread's `References` header
/// can list hundreds; the recent ones are what a reply answers.
const MAX_IDS_PER_MESSAGE: usize = 100;

/// A Message-ID in the form used for matching: no angle brackets, lowercase.
///
/// Strictly the local part is case-sensitive, but mail systems treat ids as case-insensitive
/// in practice, and a missed match only means a flag is not cleared. `None` for text that
/// cannot be an id (empty, or containing whitespace).
pub fn normalise_message_id(raw: &str) -> Option<String> {
    let id = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if id.is_empty() || id.chars().any(char::is_whitespace) {
        return None;
    }
    Some(id.to_lowercase())
}

/// The Message-IDs named by the `In-Reply-To` and `References` headers in a raw header block,
/// normalised and without duplicates.
///
/// The block is what `BODY.PEEK[HEADER.FIELDS (IN-REPLY-TO REFERENCES)]` returns. Only those
/// two headers are asked for, so the server never sends subjects or addresses. Headers may be
/// folded over several lines; they are unfolded first. Garbage in gives an empty list, not an
/// error: one odd message in Sent must not stop the harvest.
pub fn reply_ids_from_header(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);

    // Unfold: a line starting with a space or tab continues the previous header.
    let mut headers: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(last) = headers.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
        } else if !line.trim().is_empty() {
            headers.push(line.trim_end().to_owned());
        }
    }

    let mut ids: Vec<String> = Vec::new();
    for header in &headers {
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        if !(name.eq_ignore_ascii_case("in-reply-to") || name.eq_ignore_ascii_case("references")) {
            continue;
        }
        // Ids are written `<local@domain>`; anything outside the brackets (comments, old-style
        // display text) is ignored.
        let mut rest = value;
        while let Some(start) = rest.find('<') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('>') else { break };
            if let Some(id) = normalise_message_id(&after[..end]) {
                if !ids.contains(&id) && ids.len() < MAX_IDS_PER_MESSAGE {
                    ids.push(id);
                }
            }
            rest = &after[end + 1..];
        }
    }
    ids
}

// ---------------------------------------------------------------------------------------------

/// Case-insensitive flag lookup (IMAP flag names are case-insensitive).
fn has_flag(flags: &[String], wanted: &str) -> bool {
    flags.iter().any(|flag| flag.eq_ignore_ascii_case(wanted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn flags(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    // ---- plan_apply ---------------------------------------------------------------------

    #[test]
    fn an_unflagged_message_gets_the_colours_change() {
        let plan = plan_apply(&flags(&["\\Seen"]), FlagColour::Orange);
        assert_eq!(
            plan,
            ApplyPlan::Store(FlagColour::Orange.store_change().unwrap())
        );
    }

    #[test]
    fn a_flagged_message_is_never_touched_whatever_the_colour() {
        for current in [
            flags(&["\\Flagged"]),
            flags(&["\\Flagged", "$MailFlagBit0"]),
            flags(&["\\FLAGGED"]),
            flags(&["\\Seen", "\\Flagged", "$MailFlagBit1"]),
        ] {
            assert_eq!(
                plan_apply(&current, FlagColour::Orange),
                ApplyPlan::Leave(LeaveReason::AlreadyFlagged),
                "{current:?}"
            );
        }
    }

    #[test]
    fn colour_bits_without_flagged_do_not_count_as_a_user_flag() {
        // RFC 9979: the bits mean nothing without \Flagged, and the STORE sets both together.
        let plan = plan_apply(&flags(&["$MailFlagBit0"]), FlagColour::Gray);
        assert!(matches!(plan, ApplyPlan::Store(_)));
    }

    #[test]
    fn red_is_never_planned() {
        assert_eq!(
            plan_apply(&[], FlagColour::Red),
            ApplyPlan::Leave(LeaveReason::ReservedRed)
        );
    }

    #[test]
    fn no_plan_ever_adds_a_bare_flagged_without_bits_for_a_coloured_flag() {
        // Every settable colour adds \Flagged together with at least one bit, so Apple Mail
        // never sees a bare \Flagged (red) from us.
        for colour in [
            FlagColour::Orange,
            FlagColour::Yellow,
            FlagColour::Green,
            FlagColour::Blue,
            FlagColour::Purple,
            FlagColour::Gray,
        ] {
            let ApplyPlan::Store(change) = plan_apply(&[], colour) else {
                panic!("{colour:?} should be storable");
            };
            assert!(change.add.contains(&FLAGGED));
            assert!(change.add.iter().any(|f| f.starts_with("$MailFlagBit")));
        }
    }

    // ---- owned_state --------------------------------------------------------------------

    #[test]
    fn an_untouched_flag_is_intact_even_with_other_flags_around() {
        let current = flags(&["\\Seen", "\\Flagged", "$MailFlagBit0", "\\Answered"]);
        assert_eq!(owned_state(&current, FlagColour::Orange), OwnedState::Intact);
    }

    #[test]
    fn a_changed_colour_belongs_to_the_user() {
        let current = flags(&["\\Flagged", "$MailFlagBit0", "$MailFlagBit1"]); // green
        assert_eq!(
            owned_state(&current, FlagColour::Orange),
            OwnedState::ChangedByUser
        );
    }

    #[test]
    fn an_unflagged_message_belongs_to_the_user() {
        assert_eq!(
            owned_state(&flags(&["\\Seen"]), FlagColour::Orange),
            OwnedState::ChangedByUser
        );
        // The bits linger without \Flagged: still the user's doing.
        assert_eq!(
            owned_state(&flags(&["$MailFlagBit0"]), FlagColour::Orange),
            OwnedState::ChangedByUser
        );
    }

    #[test]
    fn a_red_flag_belongs_to_the_user() {
        assert_eq!(
            owned_state(&flags(&["\\Flagged"]), FlagColour::Orange),
            OwnedState::ChangedByUser
        );
    }

    // ---- clear_reason -------------------------------------------------------------------

    const TODAY: Date = date(2026, 11, 10);

    #[test]
    fn reply_needed_clears_when_replied_and_not_before() {
        let clear = |replied| clear_reason(Some(TriageFlag::ReplyNeeded), None, replied, TODAY);
        assert_eq!(clear(true), Some(ClearReason::Replied));
        assert_eq!(clear(false), None);
    }

    #[test]
    fn a_deadline_clears_the_day_after_it_and_not_on_it() {
        let clear = |deadline| clear_reason(Some(TriageFlag::Deadline), deadline, false, TODAY);
        assert_eq!(clear(Some(date(2026, 11, 9))), Some(ClearReason::DeadlinePassed));
        assert_eq!(clear(Some(date(2026, 11, 10))), None, "still the last day");
        assert_eq!(clear(Some(date(2026, 11, 11))), None);
    }

    #[test]
    fn a_deadline_without_a_date_is_never_cleared() {
        assert_eq!(
            clear_reason(Some(TriageFlag::Deadline), None, true, TODAY),
            None
        );
    }

    #[test]
    fn a_reply_does_not_clear_a_deadline_and_a_date_does_not_clear_a_reply_flag() {
        assert_eq!(
            clear_reason(
                Some(TriageFlag::Deadline),
                Some(date(2026, 12, 31)),
                true,
                TODAY
            ),
            None
        );
        assert_eq!(
            clear_reason(
                Some(TriageFlag::ReplyNeeded),
                Some(date(2020, 1, 1)),
                false,
                TODAY
            ),
            None
        );
    }

    #[test]
    fn other_flags_and_unknown_flags_wait_for_the_user() {
        for flag in [
            TriageFlag::ToPay,
            TriageFlag::Bookkeeping,
            TriageFlag::Unsure,
        ] {
            assert_eq!(
                clear_reason(Some(flag), Some(date(2020, 1, 1)), true, TODAY),
                None,
                "{flag:?}"
            );
        }
        assert_eq!(clear_reason(None, Some(date(2020, 1, 1)), true, TODAY), None);
    }

    // ---- reply ids ----------------------------------------------------------------------

    #[test]
    fn ids_are_stripped_and_lowercased() {
        assert_eq!(
            normalise_message_id(" <Abc.12@Example.ORG> ").as_deref(),
            Some("abc.12@example.org")
        );
        assert_eq!(normalise_message_id("plain@x").as_deref(), Some("plain@x"));
        assert_eq!(normalise_message_id("<>"), None);
        assert_eq!(normalise_message_id("a b"), None);
        assert_eq!(normalise_message_id(""), None);
    }

    #[test]
    fn reads_in_reply_to_and_references_from_a_header_block() {
        let raw = b"In-Reply-To: <One@x.org>\r\nReferences: <zero@x.org>\r\n <One@x.org>\r\n\r\n";
        assert_eq!(reply_ids_from_header(raw), ["one@x.org", "zero@x.org"]);
    }

    #[test]
    fn folded_and_mixed_case_headers_are_understood() {
        let raw = b"REFERENCES: <a@x>\r\n\t<b@x>\r\n  <c@x>\r\nin-reply-to: <c@x>\r\n";
        assert_eq!(reply_ids_from_header(raw), ["a@x", "b@x", "c@x"]);
    }

    #[test]
    fn other_headers_comments_and_garbage_are_ignored() {
        let raw = b"Subject: <not-an-id@x>\r\nIn-Reply-To: (comment) <ok@x> trailing\r\nBroken: <\r\n";
        assert_eq!(reply_ids_from_header(raw), ["ok@x"]);
        assert!(reply_ids_from_header(b"").is_empty());
        assert!(reply_ids_from_header(b"\xff\xfe not a header").is_empty());
        // An unterminated id is dropped without a panic.
        assert!(reply_ids_from_header(b"References: <broken").is_empty());
    }

    #[test]
    fn a_huge_references_header_is_capped() {
        let ids: String = (0..500).map(|n| format!("<id{n}@x> ")).collect();
        let raw = format!("References: {ids}\r\n");
        assert_eq!(reply_ids_from_header(raw.as_bytes()).len(), MAX_IDS_PER_MESSAGE);
    }
}
