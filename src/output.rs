//! Everything printed to stdout. Rendering returns a `String` and one write
//! sends it, so a reader that closes the pipe early (`mailctl list | head`)
//! surfaces as a `BrokenPipe` error the caller can treat as a clean exit,
//! instead of a panic inside `println!`.

use std::fmt::Write as _;
use std::io::Write as _;

use anyhow::Result;
use serde::Serialize;

use crate::model::{FolderRow, Message, MessageRow};

/// Write `text` to stdout, adding a final newline when it has none.
pub fn emit(text: &str) -> std::io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())?;
    if !text.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()
}

pub fn json<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

pub fn folders(rows: &[FolderRow]) -> String {
    let mut s = String::new();
    for r in rows {
        let unread = r
            .unseen
            .filter(|u| *u > 0)
            .map(|u| format!("  {u} unread"))
            .unwrap_or_default();
        let _ = writeln!(s, "{:<40} {:>7}{unread}", r.name, r.messages);
    }
    s
}

pub fn message_rows(rows: &[MessageRow]) -> String {
    let mut s = String::new();
    for r in rows {
        let _ = writeln!(
            s,
            "{:>6}  {:<28}  {:<38}  {}",
            r.uid,
            truncate(&sender_label(&r.from), 26),
            truncate(&r.subject, 40),
            r.date.clone().unwrap_or_default()
        );
    }
    s
}

pub fn message(msg: &Message) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "From:    {}", msg.from);
    let _ = writeln!(s, "To:      {}", msg.to);
    if !msg.cc.is_empty() {
        let _ = writeln!(s, "Cc:      {}", msg.cc);
    }
    let _ = writeln!(s, "Subject: {}", msg.subject);
    if let Some(d) = &msg.date {
        let _ = writeln!(s, "Date:    {d}");
    }
    if !msg.attachments.is_empty() {
        let _ = writeln!(s, "Files:   {}", msg.attachments.join(", "));
    }
    let _ = writeln!(s, "{}", "-".repeat(60));
    let _ = writeln!(s, "{}", msg.body);
    if msg.truncated {
        let _ = writeln!(s, "\n[body truncated]");
    }
    s
}

/// The name part of `Name <addr>`, or the address when there is no name, so a
/// narrow column shows who sent it instead of cutting an address in half.
pub fn sender_label(from: &str) -> String {
    match (from.find('<'), from.find('>')) {
        (Some(a), Some(b)) if b > a => {
            let name = from[..a].trim();
            if name.is_empty() {
                from[a + 1..b].trim().to_string()
            } else {
                name.to_string()
            }
        }
        _ => from.to_string(),
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    let one = s.replace(['\n', '\r'], " ");
    let mut out: String = one.chars().take(n).collect();
    if one.chars().count() > n {
        out.push('\u{2026}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_label_prefers_the_display_name() {
        assert_eq!(
            sender_label("Ada Lovelace <ada@example.com>"),
            "Ada Lovelace"
        );
        assert_eq!(sender_label("<bare@example.com>"), "bare@example.com");
        assert_eq!(sender_label("bare@example.com"), "bare@example.com");
        assert_eq!(sender_label(""), "");
    }

    #[test]
    fn truncate_collapses_newlines_and_marks_the_cut() {
        assert_eq!(truncate("a\nb", 10), "a b");
        assert_eq!(truncate("abcdefghij", 4), "abcd\u{2026}");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn folders_show_unread_only_when_there_are_some() {
        let rows = vec![
            FolderRow {
                name: "INBOX".into(),
                messages: 12,
                unseen: Some(3),
            },
            FolderRow {
                name: "Sent".into(),
                messages: 5,
                unseen: Some(0),
            },
        ];
        let text = folders(&rows);
        assert!(text.lines().next().unwrap().ends_with("3 unread"));
        assert!(!text.lines().nth(1).unwrap().contains("unread"));
    }

    #[test]
    fn message_marks_a_truncated_body() {
        let msg = Message {
            uid: 1,
            mailbox: "INBOX".into(),
            from: "a@x".into(),
            to: "b@y".into(),
            cc: String::new(),
            subject: "s".into(),
            date: None,
            message_id: None,
            body: "hello".into(),
            attachments: vec![],
            truncated: true,
        };
        let text = message(&msg);
        assert!(text.contains("hello"));
        assert!(text.contains("[body truncated]"));
        assert!(!text.contains("Cc:"));
    }
}
