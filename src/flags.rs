//! Apple Mail flag colours, encoded as IMAP keywords (RFC 9979).
//!
//! A colour is the `\Flagged` system flag plus three keywords, `$MailFlagBit0` to
//! `$MailFlagBit2`, that together form a 3-bit number. See `docs/FLAGS.md`.
//!
//! This module does no IO: it only translates between colours and flag names, so it is
//! easy to unit-test.

/// The IMAP system flag that marks a message as flagged.
pub const FLAGGED: &str = "\\Flagged";

/// The three colour keywords, bit 0 first.
const BIT_KEYWORDS: [&str; 3] = ["$MailFlagBit0", "$MailFlagBit1", "$MailFlagBit2"];

/// The seven colours Apple Mail can show on a flagged message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagColour {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Gray,
}

/// The flag changes needed to give a message a colour, ready for an IMAP `STORE`.
#[derive(Debug, PartialEq, Eq)]
pub struct StoreChange {
    /// Flags to add with `+FLAGS`.
    pub add: Vec<&'static str>,
    /// Flags to remove with `-FLAGS`, so no stale bit from an earlier colour is left behind.
    pub remove: Vec<&'static str>,
}

impl FlagColour {
    /// The colour's three bits, bit 0 first.
    fn bits(self) -> [bool; 3] {
        match self {
            FlagColour::Red => [false, false, false],
            FlagColour::Orange => [true, false, false],
            FlagColour::Yellow => [false, true, false],
            FlagColour::Green => [true, true, false],
            FlagColour::Blue => [false, false, true],
            FlagColour::Purple => [true, false, true],
            FlagColour::Gray => [false, true, true],
        }
    }

    /// The colour for three bits. `None` for 1-1-1, which has no colour assigned.
    fn from_bits(bits: [bool; 3]) -> Option<FlagColour> {
        match bits {
            [false, false, false] => Some(FlagColour::Red),
            [true, false, false] => Some(FlagColour::Orange),
            [false, true, false] => Some(FlagColour::Yellow),
            [true, true, false] => Some(FlagColour::Green),
            [false, false, true] => Some(FlagColour::Blue),
            [true, false, true] => Some(FlagColour::Purple),
            [false, true, true] => Some(FlagColour::Gray),
            [true, true, true] => None,
        }
    }

    /// Reads the colour from a message's current flags, as the server reported them.
    ///
    /// Returns `None` when the message is not flagged: RFC 9979 says the bits must then be
    /// ignored. IMAP flag names are case-insensitive, so the comparison is too.
    pub fn from_flags(flags: &[&str]) -> Option<FlagColour> {
        let has = |name: &str| flags.iter().any(|flag| flag.eq_ignore_ascii_case(name));

        if !has(FLAGGED) {
            return None;
        }
        let bits = BIT_KEYWORDS.map(has);
        FlagColour::from_bits(bits)
    }

    /// The flags to add and remove to give a message this colour.
    ///
    /// Returns `None` for red: red is reserved for the user, and the service never sets it.
    pub fn store_change(self) -> Option<StoreChange> {
        if self == FlagColour::Red {
            return None;
        }

        // `\Flagged` goes in the same STORE as the bits; RFC 9979 forbids setting the bits
        // on an unflagged message.
        let mut add = vec![FLAGGED];
        let mut remove = Vec::new();
        for (keyword, is_set) in BIT_KEYWORDS.into_iter().zip(self.bits()) {
            if is_set {
                add.push(keyword);
            } else {
                remove.push(keyword);
            }
        }
        Some(StoreChange { add, remove })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [FlagColour; 7] = [
        FlagColour::Red,
        FlagColour::Orange,
        FlagColour::Yellow,
        FlagColour::Green,
        FlagColour::Blue,
        FlagColour::Purple,
        FlagColour::Gray,
    ];

    #[test]
    fn orange_is_bit_0_only() {
        assert_eq!(
            FlagColour::from_flags(&["\\Flagged", "$MailFlagBit0"]),
            Some(FlagColour::Orange)
        );
    }

    #[test]
    fn flagged_without_bits_is_red() {
        assert_eq!(
            FlagColour::from_flags(&["\\Flagged"]),
            Some(FlagColour::Red)
        );
    }

    #[test]
    fn bits_are_ignored_when_not_flagged() {
        assert_eq!(FlagColour::from_flags(&["$MailFlagBit0", "\\Seen"]), None);
    }

    #[test]
    fn all_three_bits_have_no_colour() {
        let flags = [
            "\\Flagged",
            "$MailFlagBit0",
            "$MailFlagBit1",
            "$MailFlagBit2",
        ];
        assert_eq!(FlagColour::from_flags(&flags), None);
    }

    #[test]
    fn flag_names_are_case_insensitive() {
        assert_eq!(
            FlagColour::from_flags(&["\\FLAGGED", "$mailflagbit1"]),
            Some(FlagColour::Yellow)
        );
    }

    #[test]
    fn red_is_never_set() {
        assert_eq!(FlagColour::Red.store_change(), None);
    }

    #[test]
    fn store_change_sets_flagged_and_clears_stale_bits() {
        let change = FlagColour::Purple.store_change().unwrap();
        assert_eq!(change.add, ["\\Flagged", "$MailFlagBit0", "$MailFlagBit2"]);
        assert_eq!(change.remove, ["$MailFlagBit1"]);
    }

    #[test]
    fn every_settable_colour_reads_back_as_itself() {
        for colour in ALL.into_iter().filter(|&c| c != FlagColour::Red) {
            let change = colour.store_change().unwrap();
            assert_eq!(FlagColour::from_flags(&change.add), Some(colour));
        }
    }
}
