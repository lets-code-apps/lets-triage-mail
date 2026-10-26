//! Reading IMAP folder names for humans.
//!
//! IMAP (RFC 3501 §5.1.3) sends folder names in "modified UTF-7": printable ASCII as it is, and
//! everything else as UTF-16 in a base64 variant between `&` and `-`. So Gmail's Polish "Sent"
//! folder is listed as `[Gmail]/Wys&AUI-ane`, which is not what a person wants to read in
//! `check` output or a log line.
//!
//! [`decode_modified_utf7`] is **for display only**. Commands to the server must keep using the
//! raw name exactly as `LIST` returned it: the server compares the encoded form, and the
//! decoded text is not even always unique. (There is no encoder here for that reason: we never
//! have to send a decoded name back.)
//!
//! It is written by hand, in about thirty lines, because the job is small and a dependency would
//! cost more than it saves. It never fails: text it cannot decode is returned unchanged.

/// Decodes an IMAP modified-UTF-7 folder name for display.
///
/// - `&-` is a literal `&`.
/// - `&<base64>-` is UTF-16BE text, with `,` standing in for the base64 digit `/`.
/// - A name with no `&` is returned as it is.
/// - Anything malformed (a missing `-`, bad base64, an odd number of bytes, a broken UTF-16
///   surrogate pair) returns the **whole input unchanged**, so a strange name is shown as the
///   server sent it rather than half-decoded.
pub fn decode_modified_utf7(name: &str) -> String {
    decode(name).unwrap_or_else(|| name.to_owned())
}

/// The real work; `None` means "malformed".
fn decode(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find('-')?;
        let encoded = &after[..end];
        if encoded.is_empty() {
            out.push('&'); // `&-` is how an ampersand itself is written
        } else {
            out.push_str(&decode_base64_utf16(encoded)?);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Decodes the base64 part of one `&...-` run into text.
fn decode_base64_utf16(encoded: &str) -> Option<String> {
    // Collect bits six at a time and emit a byte whenever eight are ready. `u32` is plenty:
    // at most 6 + 7 bits are ever held.
    let mut bytes: Vec<u8> = Vec::new();
    let mut bits: u32 = 0;
    let mut bit_count = 0;
    for ch in encoded.chars() {
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            ',' => 63, // modified base64 uses `,` where standard base64 uses `/`
            _ => return None,
        };
        bits = (bits << 6) | value;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            bytes.push(u8::try_from((bits >> bit_count) & 0xFF).ok()?);
        }
    }

    // UTF-16 code units are two bytes each, so an odd count cannot be text.
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_unchanged() {
        for name in ["INBOX", "[Gmail]/Sent Mail", "INBOX.Sent", ""] {
            assert_eq!(decode_modified_utf7(name), name);
        }
    }

    #[test]
    fn decodes_the_gmail_polish_sent_folder() {
        assert_eq!(
            decode_modified_utf7("[Gmail]/Wys&AUI-ane"),
            "[Gmail]/Wysłane"
        );
    }

    #[test]
    fn decodes_the_examples_from_rfc_3501() {
        // RFC 3501 §5.1.3.
        assert_eq!(
            decode_modified_utf7("~peter/mail/&U,BTFw-/&ZeVnLIqe-"),
            "~peter/mail/\u{53F0}\u{5317}/\u{65E5}\u{672C}\u{8A9E}"
        );
    }

    #[test]
    fn an_ampersand_is_written_ampersand_dash() {
        assert_eq!(decode_modified_utf7("Tom &- Jerry"), "Tom & Jerry");
        assert_eq!(decode_modified_utf7("&-"), "&");
    }

    #[test]
    fn several_runs_and_surrounding_text_are_handled() {
        // "Wysłane" and "Kosz" parts in one path, plus an accented e.
        assert_eq!(decode_modified_utf7("&AUI-a/caf&AOk-/&AUE-b"), "ła/café/Łb");
    }

    #[test]
    fn characters_outside_the_basic_plane_use_surrogate_pairs() {
        // U+1F4E7 (e-mail emoji) is the surrogate pair D83D DCE7.
        assert_eq!(decode_modified_utf7("Post&2D3c5w-"), "Post\u{1F4E7}");
    }

    #[test]
    fn malformed_names_come_back_exactly_as_they_were() {
        for bad in [
            "Wys&AUI",     // no closing dash
            "Wys&A!I-ane", // not base64
            "Wys&AU-ane",  // a single byte is not a UTF-16 unit
            "Wys&2D0-x",   // a lone high surrogate
        ] {
            assert_eq!(decode_modified_utf7(bad), bad, "{bad}");
        }
    }
}
