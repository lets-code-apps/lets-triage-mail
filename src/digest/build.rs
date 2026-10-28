//! From stored rows to the digest model. No IO: rows in, a [`Digest`] out.
//!
//! The digest has these parts, in this order:
//!
//! 1. **Needs you**: every message that got (or in dry-run would have got) a flag, grouped by
//!    flag in the priority order of `docs/FLAGS.md`, one line each.
//! 2. **Noise**: counts of newsletters, marketing and cold outreach, with the noisiest senders.
//! 3. **Spam**: counts of spam and phishing.
//! 4. Everything else, as one count ("other mail").
//!
//! # Hostile input
//!
//! Senders, subjects and the model's reasons all come from email, so they are hostile. This
//! module only makes them *safe to print*: [`clean_line`] flattens each to one line without
//! control or direction-changing characters and cuts it to a length. Escaping for HTML is the
//! renderer's job, because only it knows which format it writes.
//!
//! # Accounts without flagging
//!
//! An account with `flagging = false` ([`crate::config::FlagMode::ClassifyOnly`]) never
//! chooses a flag, so its rows have none. For such an account the digest is the main output,
//! so [`DigestInput::derive_flags`] asks the engine which flag each message *would* get, from
//! the stored verdict. The sender's trust is not stored, so the engine is asked with
//! `Trust::Unknown`, which is the cautious choice: spam and phishing never need you.

use std::collections::BTreeMap;

use jiff::tz::TimeZone;
use jiff::{Timestamp, civil};

use crate::engine::{Category, FlagPolicy, TriageFlag, Trust, choose_flag};
use crate::flags::FlagColour;
use crate::store::{DigestRow, MisfiledRow};

/// Flagged messages shown per flag. More than this is a flag problem, not a reading problem;
/// the rest is reported as a count.
pub const MAX_ITEMS_PER_SECTION: usize = 25;
/// Noisy senders listed.
pub const TOP_SENDERS: usize = 5;
/// A sender needs at least this many messages to be listed as noisy: a list of one-off
/// senders says nothing.
const MIN_MESSAGES_FOR_TOP_SENDER: usize = 2;

const MAX_SENDER_CHARS: usize = 80;
const MAX_SUBJECT_CHARS: usize = 120;
const MAX_REASON_CHARS: usize = 200;

/// The order of the sections: the priority order of the flags.
pub const SECTION_ORDER: [TriageFlag; 5] = [
    TriageFlag::ReplyNeeded,
    TriageFlag::Deadline,
    TriageFlag::ToPay,
    TriageFlag::Bookkeeping,
    TriageFlag::Unsure,
];

/// Everything [`build`] needs besides the rows.
pub struct DigestInput<'a> {
    /// The account name, for the title.
    pub account: &'a str,
    /// The digest's date: today in the user's time zone.
    pub date: civil::Date,
    /// The period the rows cover, as Unix seconds (from, to).
    pub period: (i64, i64),
    /// The messages processed in that period, oldest first.
    pub rows: &'a [DigestRow],
    /// Trusted mail found in the Spam folder in that period, oldest first.
    pub misfiled: &'a [MisfiledRow],
    /// The user's time zone, for the times shown next to each message.
    pub tz: &'a TimeZone,
    /// The configured display name of a flag (`Config::flag_name`).
    pub flag_name: &'a dyn Fn(TriageFlag) -> String,
    /// The configured colour of a flag (`FlagPolicy::colour_for`), for the dot next to it.
    pub flag_colour: &'a dyn Fn(TriageFlag) -> FlagColour,
    /// `Some` for a classify-only account: choose each message's would-be flag with this
    /// policy, because no flag was stored. `None` uses the stored flags.
    pub derive_flags: Option<&'a FlagPolicy>,
    /// The account is in dry-run: the flags shown were not set on the server.
    pub dry_run: bool,
}

/// One line in the "needs you" part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub sender: String,
    pub subject: String,
    /// The model's one-sentence reason; may be empty.
    pub reason: String,
    /// The date to act by, for a "Deadline" flag.
    pub deadline: Option<civil::Date>,
    /// When the message was processed, in the user's time zone.
    pub received: civil::DateTime,
}

/// All messages with one flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub flag: TriageFlag,
    pub colour: FlagColour,
    /// The configured display name, for example "Reply needed".
    pub title: String,
    /// At most [`MAX_ITEMS_PER_SECTION`] items.
    pub items: Vec<Item>,
    /// How many more there were.
    pub hidden: usize,
}

impl Section {
    /// Items shown plus items hidden.
    pub fn total(&self) -> usize {
        self.items.len() + self.hidden
    }
}

/// A sender and how many messages it sent in the period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderCount {
    pub sender: String,
    pub count: usize,
}

/// The messages that did not need the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Noise {
    pub newsletters: usize,
    pub marketing: usize,
    pub cold_outreach: usize,
    pub spam: usize,
    pub phishing: usize,
    /// Everything else without a flag: personal or transactional mail that needs nothing,
    /// and mail the model did not judge.
    pub other: usize,
    /// The senders behind the newsletters, marketing, cold outreach and spam.
    pub top_senders: Vec<SenderCount>,
}

impl Noise {
    /// Newsletters, marketing and cold outreach.
    pub fn bulk(&self) -> usize {
        self.newsletters + self.marketing + self.cold_outreach
    }

    /// Spam and phishing.
    pub fn junk(&self) -> usize {
        self.spam + self.phishing
    }
}

/// The digest of one account for one period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub account: String,
    pub date: civil::Date,
    /// The period covered, in the user's time zone.
    pub from: civil::DateTime,
    pub to: civil::DateTime,
    pub dry_run: bool,
    /// A classify-only account: the flags shown were never set, by design.
    pub classify_only: bool,
    /// Messages in the period, of every kind.
    pub total: usize,
    /// Only the flags that have messages, in [`SECTION_ORDER`].
    pub sections: Vec<Section>,
    pub noise: Noise,
    /// Trusted mail found in the Spam folder, at most [`MAX_ITEMS_PER_SECTION`]. The `reason`
    /// is empty and `received` is the message's own date when it had one.
    pub misfiled: Vec<Item>,
    /// How many more misfiled messages there were.
    pub misfiled_hidden: usize,
}

impl Digest {
    /// How many messages need the user.
    pub fn needs_you(&self) -> usize {
        self.sections.iter().map(Section::total).sum()
    }
}

/// Builds the digest model. Pure: the same input always gives the same digest.
pub fn build(input: &DigestInput<'_>) -> Digest {
    let local = |seconds: i64| {
        Timestamp::from_second(seconds)
            .unwrap_or(Timestamp::UNIX_EPOCH)
            .to_zoned(input.tz.clone())
            .datetime()
    };

    // Group the flagged messages and count the rest in one pass. The map is keyed by the
    // flag's place in `SECTION_ORDER`, so iterating it later gives the sections in order.
    let mut flagged: BTreeMap<usize, Vec<Item>> = BTreeMap::new();
    let mut noise = Noise::default();
    let mut sender_counts: BTreeMap<String, usize> = BTreeMap::new();

    for row in input.rows {
        let flag = row.flag.or_else(|| would_be_flag(row, input.derive_flags));
        if let Some(flag) = flag {
            let position = SECTION_ORDER.iter().position(|&f| f == flag).unwrap_or(0);
            flagged.entry(position).or_default().push(Item {
                sender: clean_line(row.sender.as_deref().unwrap_or(""), MAX_SENDER_CHARS),
                subject: clean_line(row.subject.as_deref().unwrap_or(""), MAX_SUBJECT_CHARS),
                reason: clean_line(
                    row.verdict.as_ref().map_or("", |v| v.reason.as_str()),
                    MAX_REASON_CHARS,
                ),
                deadline: row.verdict.as_ref().and_then(|v| v.deadline),
                received: local(row.created_at),
            });
            continue;
        }

        let noisy = match row.verdict.as_ref().map(|v| v.category) {
            Some(Category::Newsletter) => {
                noise.newsletters += 1;
                true
            }
            Some(Category::Marketing) => {
                noise.marketing += 1;
                true
            }
            Some(Category::ColdOutreach) => {
                noise.cold_outreach += 1;
                true
            }
            Some(Category::Spam) => {
                noise.spam += 1;
                true
            }
            Some(Category::Phishing) => {
                noise.phishing += 1;
                true
            }
            Some(Category::Personal | Category::Transactional) | None => {
                noise.other += 1;
                false
            }
        };
        if noisy && let Some(sender) = row.sender.as_deref() {
            let sender = clean_line(sender, MAX_SENDER_CHARS).to_lowercase();
            if !sender.is_empty() {
                *sender_counts.entry(sender).or_default() += 1;
            }
        }
    }
    noise.top_senders = top_senders(sender_counts);

    let sections = flagged
        .into_iter()
        .map(|(position, mut items)| {
            let flag = SECTION_ORDER[position];
            if flag == TriageFlag::Deadline {
                // Soonest deadline first, messages without a date last (a plain `Option` sort
                // would put `None` first). The sort is stable: equal keys keep arrival order.
                items.sort_by_key(|item| (item.deadline.is_none(), item.deadline));
            }
            let hidden = items.len().saturating_sub(MAX_ITEMS_PER_SECTION);
            items.truncate(MAX_ITEMS_PER_SECTION);
            Section {
                flag,
                colour: (input.flag_colour)(flag),
                title: clean_line(&(input.flag_name)(flag), MAX_SENDER_CHARS),
                items,
                hidden,
            }
        })
        .collect();

    let mut misfiled: Vec<Item> = input
        .misfiled
        .iter()
        .map(|row| Item {
            sender: clean_line(&row.sender, MAX_SENDER_CHARS),
            subject: clean_line(&row.subject, MAX_SUBJECT_CHARS),
            reason: String::new(),
            deadline: None,
            // The message's own date when it parses, otherwise when we found it.
            received: row
                .date
                .as_deref()
                .and_then(|date| date.parse::<Timestamp>().ok())
                .map_or_else(
                    || local(row.created_at),
                    |at| at.to_zoned(input.tz.clone()).datetime(),
                ),
        })
        .collect();
    let misfiled_hidden = misfiled.len().saturating_sub(MAX_ITEMS_PER_SECTION);
    misfiled.truncate(MAX_ITEMS_PER_SECTION);

    Digest {
        account: clean_line(input.account, MAX_SENDER_CHARS),
        date: input.date,
        from: local(input.period.0),
        to: local(input.period.1),
        dry_run: input.dry_run,
        classify_only: input.derive_flags.is_some(),
        total: input.rows.len(),
        sections,
        noise,
        misfiled,
        misfiled_hidden,
    }
}

/// The flag a classify-only account's message would get. `None` when stored flags are used.
fn would_be_flag(row: &DigestRow, policy: Option<&FlagPolicy>) -> Option<TriageFlag> {
    let policy = policy?;
    let verdict = row.verdict.as_ref()?;
    choose_flag(Trust::Unknown, verdict, policy)
}

/// The noisiest senders, most messages first, ties by address so the output is stable.
fn top_senders(counts: BTreeMap<String, usize>) -> Vec<SenderCount> {
    let mut senders: Vec<SenderCount> = counts
        .into_iter()
        .filter(|&(_, count)| count >= MIN_MESSAGES_FOR_TOP_SENDER)
        .map(|(sender, count)| SenderCount { sender, count })
        .collect();
    // `BTreeMap` iterates in key order and the sort is stable, so ties stay alphabetical.
    senders.sort_by_key(|s| std::cmp::Reverse(s.count));
    senders.truncate(TOP_SENDERS);
    senders
}

/// Makes untrusted text safe to print on one line: control characters (newlines, tabs, escape
/// codes) become spaces, invisible and direction-changing characters are dropped, runs of
/// whitespace collapse, and the result is cut to `max_chars` with an ellipsis.
///
/// The direction-changing characters (U+202A to U+202E, U+2066 to U+2069) can make text read
/// in a different order than it is written, which lets a sender disguise a line. Dropping
/// them, and the zero-width ones, costs nothing for honest mail.
pub fn clean_line(text: &str, max_chars: usize) -> String {
    let visible: String = text
        .chars()
        .filter(|&c| !is_invisible_format_char(c))
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = visible.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    // Keep room for the ellipsis so the result never exceeds `max_chars`.
    let mut cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

fn is_invisible_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' // zero-width space and joiners, left/right marks
        | '\u{202A}'..='\u{202E}' // embeddings and overrides
        | '\u{2060}'..='\u{2064}' // word joiner and invisible operators
        | '\u{2066}'..='\u{2069}' // isolates
        | '\u{FEFF}' // byte order mark
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::engine::{Payment, Verdict};

    fn verdict(category: Category, reason: &str) -> Verdict {
        Verdict {
            category,
            needs_reply: false,
            deadline: None,
            payment: Payment::NotApplicable,
            confidence: 0.9,
            reason: reason.into(),
        }
    }

    fn row(
        sender: &str,
        subject: &str,
        flag: Option<TriageFlag>,
        verdict: Option<Verdict>,
        created_at: i64,
    ) -> DigestRow {
        DigestRow {
            sender: Some(sender.into()),
            subject: Some(subject.into()),
            flag,
            verdict,
            created_at,
        }
    }

    fn build_with(rows: &[DigestRow], derive: Option<&FlagPolicy>) -> Digest {
        let config = Config::default();
        build(&DigestInput {
            account: "work",
            date: civil::date(2026, 10, 26),
            // 2026-10-25 07:00 to 2026-10-26 07:00 UTC.
            period: (1_792_911_600, 1_792_998_000),
            rows,
            misfiled: &[],
            tz: &TimeZone::UTC,
            flag_name: &|flag| config.flag_name(flag).to_owned(),
            flag_colour: &|flag| flag.colour(),
            derive_flags: derive,
            dry_run: false,
        })
    }

    #[test]
    fn flagged_messages_are_grouped_in_priority_order() {
        let rows = [
            row("a@x.com", "Gray", Some(TriageFlag::Unsure), None, 1),
            row("b@x.com", "Reply", Some(TriageFlag::ReplyNeeded), None, 2),
            row("c@x.com", "Pay", Some(TriageFlag::ToPay), None, 3),
            row("d@x.com", "Reply 2", Some(TriageFlag::ReplyNeeded), None, 4),
        ];
        let digest = build_with(&rows, None);
        let titles: Vec<_> = digest.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Reply needed", "To pay", "Unsure"]);
        let reply: Vec<_> = digest.sections[0]
            .items
            .iter()
            .map(|i| i.subject.as_str())
            .collect();
        assert_eq!(reply, ["Reply", "Reply 2"]);
        assert_eq!(digest.needs_you(), 4);
    }

    #[test]
    fn deadlines_are_sorted_soonest_first_and_undated_last() {
        let dated = |day: i8| {
            Some(Verdict {
                deadline: Some(civil::date(2026, 11, day)),
                ..verdict(Category::Transactional, "")
            })
        };
        let rows = [
            row("a@x.com", "none", Some(TriageFlag::Deadline), None, 1),
            row("b@x.com", "late", Some(TriageFlag::Deadline), dated(20), 2),
            row("c@x.com", "soon", Some(TriageFlag::Deadline), dated(2), 3),
        ];
        let digest = build_with(&rows, None);
        let subjects: Vec<_> = digest.sections[0]
            .items
            .iter()
            .map(|i| i.subject.as_str())
            .collect();
        assert_eq!(subjects, ["soon", "late", "none"]);
    }

    #[test]
    fn unflagged_mail_is_counted_by_kind() {
        let kind = |sender: &str, category: Option<Category>| {
            row(sender, "s", None, category.map(|c| verdict(c, "")), 1)
        };
        let rows = [
            kind("n@x.com", Some(Category::Newsletter)),
            kind("n@x.com", Some(Category::Newsletter)),
            kind("m@x.com", Some(Category::Marketing)),
            kind("c@x.com", Some(Category::ColdOutreach)),
            kind("s@x.com", Some(Category::Spam)),
            kind("p@x.com", Some(Category::Phishing)),
            kind("f@x.com", Some(Category::Personal)),
            kind("g@x.com", None),
        ];
        let noise = build_with(&rows, None).noise;
        assert_eq!(
            (noise.newsletters, noise.marketing, noise.cold_outreach),
            (2, 1, 1)
        );
        assert_eq!((noise.spam, noise.phishing, noise.other), (1, 1, 2));
        // Only senders with two or more messages are listed.
        assert_eq!(
            noise.top_senders,
            [SenderCount {
                sender: "n@x.com".into(),
                count: 2
            }]
        );
    }

    #[test]
    fn long_sections_are_cut_and_the_rest_counted() {
        let rows: Vec<_> = (0..30)
            .map(|i| {
                row(
                    "a@x.com",
                    &format!("m{i}"),
                    Some(TriageFlag::ReplyNeeded),
                    None,
                    i,
                )
            })
            .collect();
        let section = &build_with(&rows, None).sections[0];
        assert_eq!(section.items.len(), MAX_ITEMS_PER_SECTION);
        assert_eq!(section.hidden, 5);
        assert_eq!(section.total(), 30);
    }

    #[test]
    fn a_classify_only_account_gets_would_be_flags_from_the_verdict() {
        let asks = Verdict {
            needs_reply: true,
            ..verdict(Category::Personal, "asks a question")
        };
        let spam_asks = Verdict {
            needs_reply: true,
            ..verdict(Category::Spam, "pretends to ask")
        };
        let rows = [
            row("a@x.com", "Hello", None, Some(asks), 1),
            row("b@x.com", "Win", None, Some(spam_asks), 2),
        ];
        let policy = Config::default().flag_policy();

        // Without the policy nothing needs the user: no flag was stored.
        assert_eq!(build_with(&rows, None).needs_you(), 0);

        let digest = build_with(&rows, Some(&policy));
        assert_eq!(digest.needs_you(), 1);
        assert_eq!(digest.sections[0].items[0].subject, "Hello");
        // Spam never needs you, whatever it claims.
        assert_eq!(digest.noise.spam, 1);
    }

    #[test]
    fn times_are_shown_in_the_users_time_zone() {
        let rows = [row(
            "a@x.com",
            "S",
            Some(TriageFlag::Unsure),
            None,
            1_792_911_600,
        )];
        let config = Config::default();
        let tz = TimeZone::get("Europe/Warsaw").unwrap();
        let digest = build(&DigestInput {
            account: "work",
            date: civil::date(2026, 10, 25),
            period: (1_792_911_600, 1_792_998_000),
            rows: &rows,
            misfiled: &[],
            tz: &tz,
            flag_name: &|flag| config.flag_name(flag).to_owned(),
            flag_colour: &|flag| flag.colour(),
            derive_flags: None,
            dry_run: false,
        });
        // 07:00 UTC is 08:00 in Warsaw on 25 October 2026 (daylight saving ended that night).
        assert_eq!(digest.sections[0].items[0].received.hour(), 8);
    }

    #[test]
    fn clean_line_flattens_and_cuts() {
        assert_eq!(clean_line("a\r\nb\tc  d", 50), "a b c d");
        assert_eq!(clean_line("a\u{202E}b\u{200B}c\u{1b}[31m", 50), "abc [31m");
        assert_eq!(clean_line("abcdefghij", 5), "abcd…");
        assert_eq!(clean_line("abc", 5), "abc");
        assert_eq!(clean_line("   ", 5), "");
        // Characters, not bytes: never splits a multi-byte character.
        assert_eq!(clean_line("ééééé", 3), "éé…");
    }

    fn misfiled_row(sender: &str, date: Option<&str>, created_at: i64) -> MisfiledRow {
        MisfiledRow {
            sender: sender.into(),
            subject: "Lunch?".into(),
            date: date.map(String::from),
            created_at,
        }
    }

    fn build_with_misfiled(misfiled: &[MisfiledRow]) -> Digest {
        let config = Config::default();
        build(&DigestInput {
            account: "work",
            date: civil::date(2026, 10, 26),
            period: (1_792_911_600, 1_792_998_000),
            rows: &[],
            misfiled,
            tz: &TimeZone::UTC,
            flag_name: &|flag| config.flag_name(flag).to_owned(),
            flag_colour: &|flag| flag.colour(),
            derive_flags: None,
            dry_run: false,
        })
    }

    #[test]
    fn misfiled_mail_uses_the_message_date_when_it_has_one() {
        let rows = [
            misfiled_row(
                "anna@friend.example",
                Some("2026-10-25T09:30:00Z"),
                1_792_911_600,
            ),
            // No date: the time we found it, 2026-10-25 07:01 UTC.
            misfiled_row("bob@friend.example", None, 1_792_911_660),
        ];
        let digest = build_with_misfiled(&rows);
        assert_eq!(digest.misfiled.len(), 2);
        assert_eq!(digest.misfiled_hidden, 0);
        assert_eq!(digest.misfiled[0].sender, "anna@friend.example");
        assert_eq!(
            digest.misfiled[0].received,
            civil::date(2026, 10, 25).at(9, 30, 0, 0)
        );
        assert_eq!(
            digest.misfiled[1].received,
            civil::date(2026, 10, 25).at(7, 1, 0, 0)
        );
        assert!(digest.misfiled[0].reason.is_empty());
        // Misfiled mail is not counted as classified mail.
        assert_eq!(digest.total, 0);
    }

    #[test]
    fn misfiled_text_is_cleaned_and_the_list_is_capped() {
        let mut rows = vec![misfiled_row("evil\r\nname@x.example", None, 1_792_911_600)];
        for i in 0..30 {
            rows.push(misfiled_row(
                &format!("p{i}@x.example"),
                None,
                1_792_911_600,
            ));
        }
        let digest = build_with_misfiled(&rows);
        assert_eq!(digest.misfiled[0].sender, "evil name@x.example");
        assert_eq!(digest.misfiled.len(), MAX_ITEMS_PER_SECTION);
        assert_eq!(digest.misfiled_hidden, 31 - MAX_ITEMS_PER_SECTION);
    }
}
