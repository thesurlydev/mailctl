//! The data `mailctl` returns. The JSON field names here are a contract: agents
//! and scripts parse them, so a rename is a breaking change.

use mail_parser::{AddressList, Mailbox};
use serde::Serialize;

#[derive(Serialize, Debug, PartialEq)]
pub struct FolderRow {
    pub name: String,
    pub messages: u32,
    pub unseen: Option<u32>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct MessageRow {
    pub uid: u32,
    pub from: String,
    pub subject: String,
    /// The time the server received the message (IMAP INTERNALDATE).
    pub date: Option<String>,
    pub seen: bool,
    pub flagged: bool,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Message {
    pub uid: u32,
    pub mailbox: String,
    pub from: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    /// The time the server received the message (IMAP INTERNALDATE).
    pub date: Option<String>,
    pub message_id: Option<String>,
    /// Untrusted text written by whoever sent the message.
    pub body: String,
    pub attachments: Vec<String>,
    /// True when `body` was cut at the body limit.
    pub truncated: bool,
    /// The links in the HTML part, only when `read --links` asked for them, so
    /// the field is absent otherwise and the default JSON is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<Link>>,
}

/// An anchor from an HTML message: its visible text and its `http(s)` URL.
/// Untrusted text written by whoever sent the message.
#[derive(Serialize, Debug, PartialEq)]
pub struct Link {
    pub text: String,
    pub url: String,
}

/// `Name <addr>`, or just the address when the display name is absent.
pub fn format_mailbox(m: &Mailbox<'_>) -> Option<String> {
    let addr = m.address()?;
    match m.name() {
        Some(name) if !name.is_empty() => Some(format!("{name} <{addr}>")),
        _ => Some(addr.to_string()),
    }
}

/// An address header as one string, `a <a@x>, b@y`. Group members are
/// flattened, since a caller wants the people and not the grouping.
pub fn addr_list(list: Option<AddressList<'_>>) -> String {
    list.map(|l| {
        l.mailboxes()
            .filter_map(|m| format_mailbox(&m))
            .collect::<Vec<_>>()
            .join(", ")
    })
    .unwrap_or_default()
}

/// Split a `Name <addr>` header into the address, which is what a caller
/// usually wants to match on.
pub fn address_only(s: &str) -> String {
    match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if b > a => s[a + 1..b].trim().to_string(),
        _ => s.trim().to_string(),
    }
}

/// The first `limit` characters of `text`, and whether anything was cut.
/// `limit` counts characters, never bytes, so a multi-byte character is never
/// split. A limit of 0 means no limit.
pub fn truncate_chars(text: String, limit: usize) -> (String, bool) {
    if limit == 0 {
        return (text, false);
    }
    match text.char_indices().nth(limit) {
        Some((byte, _)) => (text[..byte].to_string(), true),
        None => (text, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_only_unwraps_a_display_name() {
        assert_eq!(
            address_only("Ada Lovelace <ada@example.com>"),
            "ada@example.com"
        );
        assert_eq!(address_only("ada@example.com"), "ada@example.com");
        assert_eq!(address_only("  spaced@example.com  "), "spaced@example.com");
    }

    #[test]
    fn truncate_chars_never_splits_a_multibyte_character() {
        // 19_999 ASCII bytes then a 3-byte character straddling a 20_000 byte
        // cut: the case that panicked when the limit counted bytes.
        let mut body = "a".repeat(19_999);
        body.push('\u{20ac}');
        body.push_str("tail");
        let (out, cut) = truncate_chars(body, 20_000);
        assert!(cut);
        assert_eq!(out.chars().count(), 20_000);
        assert!(out.ends_with('\u{20ac}'));
    }

    #[test]
    fn truncate_chars_leaves_short_text_alone() {
        assert_eq!(truncate_chars("abc".into(), 3), ("abc".into(), false));
        assert_eq!(truncate_chars("abcd".into(), 3), ("abc".into(), true));
        assert_eq!(truncate_chars(String::new(), 3), (String::new(), false));
    }

    #[test]
    fn truncate_chars_zero_means_unlimited() {
        let text = "x".repeat(50_000);
        assert_eq!(truncate_chars(text.clone(), 0), (text, false));
    }

    #[test]
    fn json_field_names_are_the_contract() {
        let row = MessageRow {
            uid: 7,
            from: "Ada <ada@example.com>".into(),
            subject: "Hi".into(),
            date: Some("2026-11-17 09:30".into()),
            seen: false,
            flagged: true,
        };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({
                "uid": 7,
                "from": "Ada <ada@example.com>",
                "subject": "Hi",
                "date": "2026-11-17 09:30",
                "seen": false,
                "flagged": true,
            })
        );
        let folder = FolderRow {
            name: "INBOX".into(),
            messages: 3,
            unseen: None,
        };
        assert_eq!(
            serde_json::to_value(&folder).unwrap(),
            serde_json::json!({"name": "INBOX", "messages": 3, "unseen": null})
        );
    }
}
