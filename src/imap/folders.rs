//! Choosing folders from `LIST` output.
//!
//! Pure functions over plain data, so they are tested without a server. The session turns the
//! server's `LIST` response into [`ListedFolder`]s and calls [`resolve_folders`].
//!
//! # Why we infer the namespace prefix
//!
//! RFC 2342 `NAMESPACE` tells a client that, say, the personal folders live under `INBOX.`
//! (a common Dovecot and Courier setup). `async-imap` has no `NAMESPACE` command, so we infer
//! the same thing from `LIST`: if folders exist below `INBOX<delimiter>`, that is the prefix.
//! We never invent folder names. We only pick among folders the server actually listed.

/// One line of a `LIST` response, reduced to what we need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedFolder {
    /// The full folder name as the server wants it in commands.
    pub name: String,
    /// The hierarchy delimiter, `None` for flat servers.
    pub delimiter: Option<String>,
    /// Attributes such as `\Sent` or `\Noselect`, as the server sent them.
    pub attributes: Vec<String>,
}

impl ListedFolder {
    fn has_attribute(&self, wanted: &str) -> bool {
        self.attributes
            .iter()
            .any(|a| a.eq_ignore_ascii_case(wanted))
    }
}

/// The folders the service cares about in v1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFolders {
    /// Always `INBOX`: the name is special and case-insensitive in IMAP.
    pub inbox: String,
    /// The Sent folder, or `None` if the server has none we can recognise.
    pub sent: Option<String>,
    /// The Spam folder, or `None` if the server has none we can recognise.
    pub junk: Option<String>,
    /// The hierarchy delimiter, if the server has one.
    pub delimiter: Option<String>,
    /// The personal namespace prefix, such as `INBOX.`; empty when folders live at top level.
    pub prefix: String,
}

/// Plain names people and mail clients use for the Sent folder, most common first.
const SENT_NAMES: [&str; 5] = [
    "Sent",
    "Sent Items",
    "Sent Messages",
    "Sent Mail",
    "Gesendet",
];

/// Plain names used for the Spam folder, most common first. `Spam` is what wp.pl uses.
const JUNK_NAMES: [&str; 4] = ["Junk", "Spam", "Junk E-mail", "Junk Email"];

/// Find the inbox, the Sent folder and the Spam folder.
///
/// Order for Sent and Spam: a folder with the special-use attribute (`\Sent` or `\Junk`,
/// RFC 6154), then the common names, both below the personal namespace prefix and at top level.
pub fn resolve_folders(listed: &[ListedFolder]) -> ResolvedFolders {
    let delimiter = listed
        .iter()
        .find(|f| f.name.eq_ignore_ascii_case("INBOX"))
        .and_then(|f| f.delimiter.clone())
        .or_else(|| listed.iter().find_map(|f| f.delimiter.clone()));

    let prefix = namespace_prefix(listed, delimiter.as_deref());

    ResolvedFolders {
        inbox: "INBOX".to_string(),
        sent: find_folder(listed, &prefix, "\\Sent", &SENT_NAMES),
        junk: find_folder(listed, &prefix, "\\Junk", &JUNK_NAMES),
        delimiter,
        prefix,
    }
}

/// The selectable folder with the special-use `attribute`, or else the first of `names` that
/// exists, tried with the namespace `prefix` first and then bare.
fn find_folder(
    listed: &[ListedFolder],
    prefix: &str,
    attribute: &str,
    names: &[&str],
) -> Option<String> {
    let selectable = |f: &&ListedFolder| !f.has_attribute("\\Noselect");

    let by_special_use = listed
        .iter()
        .filter(selectable)
        .find(|f| f.has_attribute(attribute))
        .map(|f| f.name.clone());

    by_special_use.or_else(|| {
        names.iter().find_map(|plain| {
            let prefixed = format!("{prefix}{plain}");
            // Try the prefixed spelling first, then the bare one.
            [prefixed.as_str(), plain].iter().find_map(|candidate| {
                listed
                    .iter()
                    .filter(selectable)
                    .find(|f| f.name.eq_ignore_ascii_case(candidate))
                    .map(|f| f.name.clone())
            })
        })
    })
}

/// `INBOX.` when folders exist below `INBOX` + delimiter, otherwise empty.
fn namespace_prefix(listed: &[ListedFolder], delimiter: Option<&str>) -> String {
    let Some(delimiter) = delimiter else {
        return String::new();
    };
    let prefix = format!("INBOX{delimiter}");
    let has_children = listed.iter().any(|f| {
        f.name.len() > prefix.len()
            && f.name
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(&prefix))
    });
    if has_children { prefix } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, delimiter: &str, attributes: &[&str]) -> ListedFolder {
        ListedFolder {
            name: name.into(),
            delimiter: Some(delimiter.into()),
            attributes: attributes.iter().map(|a| a.to_string()).collect(),
        }
    }

    #[test]
    fn special_use_wins_over_names() {
        let listed = [
            folder("INBOX", "/", &[]),
            folder("Sent", "/", &[]),
            folder("Verschickt", "/", &["\\Sent"]),
        ];
        assert_eq!(resolve_folders(&listed).sent.as_deref(), Some("Verschickt"));
    }

    #[test]
    fn special_use_is_matched_case_insensitively() {
        let listed = [folder("INBOX", "/", &[]), folder("Out", "/", &["\\sent"])];
        assert_eq!(resolve_folders(&listed).sent.as_deref(), Some("Out"));
    }

    #[test]
    fn falls_back_to_common_names() {
        let listed = [folder("INBOX", "/", &[]), folder("Sent Items", "/", &[])];
        assert_eq!(resolve_folders(&listed).sent.as_deref(), Some("Sent Items"));
    }

    #[test]
    fn respects_the_inbox_prefix_and_delimiter() {
        let listed = [
            folder("INBOX", ".", &[]),
            folder("INBOX.Drafts", ".", &[]),
            folder("INBOX.Sent", ".", &[]),
        ];
        let resolved = resolve_folders(&listed);
        assert_eq!(resolved.sent.as_deref(), Some("INBOX.Sent"));
        assert_eq!(resolved.delimiter.as_deref(), Some("."));
        assert_eq!(resolved.prefix, "INBOX.");
    }

    #[test]
    fn top_level_folders_have_no_prefix() {
        let listed = [folder("INBOX", "/", &[]), folder("Sent", "/", &[])];
        let resolved = resolve_folders(&listed);
        assert_eq!(resolved.prefix, "");
        assert_eq!(resolved.sent.as_deref(), Some("Sent"));
    }

    #[test]
    fn name_match_ignores_case_and_keeps_the_server_spelling() {
        let listed = [folder("INBOX", "/", &[]), folder("SENT", "/", &[])];
        assert_eq!(resolve_folders(&listed).sent.as_deref(), Some("SENT"));
    }

    #[test]
    fn unselectable_folders_are_skipped() {
        let listed = [
            folder("INBOX", "/", &[]),
            folder("Sent", "/", &["\\Noselect"]),
        ];
        assert_eq!(resolve_folders(&listed).sent, None);
    }

    #[test]
    fn no_sent_folder_means_none() {
        let listed = [folder("INBOX", "/", &[]), folder("Archive", "/", &[])];
        assert_eq!(resolve_folders(&listed).sent, None);
    }

    #[test]
    fn inbox_is_always_inbox() {
        assert_eq!(resolve_folders(&[]).inbox, "INBOX");
    }

    #[test]
    fn junk_is_found_by_special_use() {
        let listed = [
            folder("INBOX", "/", &[]),
            folder("Junk", "/", &[]),
            folder("Unwanted", "/", &["\\Junk"]),
        ];
        assert_eq!(resolve_folders(&listed).junk.as_deref(), Some("Unwanted"));
    }

    #[test]
    fn junk_falls_back_to_the_spam_name() {
        let listed = [folder("INBOX", "/", &[]), folder("Spam", "/", &[])];
        assert_eq!(resolve_folders(&listed).junk.as_deref(), Some("Spam"));
    }

    #[test]
    fn junk_respects_the_inbox_prefix() {
        let listed = [folder("INBOX", ".", &[]), folder("INBOX.Junk", ".", &[])];
        assert_eq!(resolve_folders(&listed).junk.as_deref(), Some("INBOX.Junk"));
    }

    #[test]
    fn no_junk_folder_means_none() {
        let listed = [folder("INBOX", "/", &[]), folder("Sent", "/", &["\\Sent"])];
        assert_eq!(resolve_folders(&listed).junk, None);
    }
}
