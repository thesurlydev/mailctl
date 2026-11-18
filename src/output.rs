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
        let _ = writeln!(s, "{:<40} {:>7}{unread}", line(&r.name), r.messages);
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
            line(r.date.as_deref().unwrap_or_default())
        );
    }
    s
}

/// Everything in a message is written by its sender, so every field goes
/// through `line` or `block` on its way to the terminal.
pub fn message(msg: &Message) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "From:    {}", line(&msg.from));
    let _ = writeln!(s, "To:      {}", line(&msg.to));
    if !msg.cc.is_empty() {
        let _ = writeln!(s, "Cc:      {}", line(&msg.cc));
    }
    let _ = writeln!(s, "Subject: {}", line(&msg.subject));
    if let Some(d) = &msg.date {
        let _ = writeln!(s, "Date:    {}", line(d));
    }
    if !msg.attachments.is_empty() {
        let _ = writeln!(s, "Files:   {}", line(&msg.attachments.join(", ")));
    }
    let _ = writeln!(s, "{}", "-".repeat(60));
    let _ = writeln!(s, "{}", block(&msg.body));
    if msg.truncated {
        let _ = writeln!(s, "\n[body truncated]");
    }
    if let Some(links) = &msg.links {
        let _ = writeln!(s, "\nLinks:");
        if links.is_empty() {
            let _ = writeln!(s, "  (none)");
        }
        for l in links {
            if l.text.is_empty() {
                let _ = writeln!(s, "  {}", line(&l.url));
            } else {
                let _ = writeln!(s, "  {}  {}", line(&l.text), line(&l.url));
            }
        }
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

/// Text written by a stranger, made safe to print to a terminal on one line:
/// a run of line breaks or tabs becomes one space (a folded header), and any
/// other character that could drive the terminal shows as U+FFFD, so it is
/// visible instead of acting or hiding.
pub fn line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_break = false;
    for c in s.chars() {
        if matches!(c, '\r' | '\n' | '\t') {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
            continue;
        }
        in_break = false;
        out.push(if is_terminal_control(c) {
            '\u{fffd}'
        } else {
            c
        });
    }
    out
}

/// Like `line`, for a body: newlines and tabs are kept, and a CRLF line end
/// becomes a newline. A lone carriage return, which would let the next text
/// overwrite the line, shows as U+FFFD like every other control.
pub fn block(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\t' => out.push(c),
            c if is_terminal_control(c) => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

/// A control character (escape, bell, carriage return, the C1 set) or a
/// bidi control, which can reorder what the reader sees, for example to make
/// `exe.txt` read as `txt.exe`.
fn is_terminal_control(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

/// `line`, cut to `n` characters with an ellipsis when it was longer.
pub fn truncate(s: &str, n: usize) -> String {
    let one = line(s);
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
    fn line_folds_breaks_and_shows_controls() {
        assert_eq!(line("a\r\n\tb"), "a b");
        assert_eq!(line("x\u{1b}[2Jy\u{7}"), "x\u{fffd}[2Jy\u{fffd}");
        assert_eq!(line("C1\u{9b}31m"), "C1\u{fffd}31m");
        assert_eq!(line("\u{202e}txt.exe"), "\u{fffd}txt.exe");
        assert_eq!(line("Caf\u{e9} \u{2014} ok"), "Caf\u{e9} \u{2014} ok");
    }

    #[test]
    fn block_keeps_lines_and_tabs_and_shows_a_lone_carriage_return() {
        assert_eq!(block("a\r\nb\n\tc"), "a\nb\n\tc");
        assert_eq!(block("safe\rOVER"), "safe\u{fffd}OVER");
        assert_eq!(block("\u{1b}[31mred"), "\u{fffd}[31mred");
    }

    #[test]
    fn message_rows_show_controls_and_stay_on_one_line() {
        let rows = vec![MessageRow {
            uid: 7,
            from: "Evil\u{1b}]0;t\u{7} <e@example.com>".into(),
            subject: "Hi\r\nthere\u{1b}[2J".into(),
            date: Some("2026-11-17 09:30".into()),
            seen: false,
            flagged: false,
        }];
        let text = message_rows(&rows);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("Evil\u{fffd}]0;t\u{fffd}"), "{text:?}");
        assert!(text.contains("Hi there\u{fffd}[2J"), "{text:?}");
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
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
            links: None,
        };
        let text = message(&msg);
        assert!(text.contains("hello"));
        assert!(text.contains("[body truncated]"));
        assert!(!text.contains("Cc:"));
        assert!(!text.contains("Links:"));
    }

    #[test]
    fn message_lists_links_only_when_asked() {
        let mut msg = Message {
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
            truncated: false,
            links: Some(vec![
                crate::model::Link {
                    text: "Staff Engineer".into(),
                    url: "https://x.example/1".into(),
                },
                crate::model::Link {
                    text: String::new(),
                    url: "https://x.example/2".into(),
                },
            ]),
        };
        let text = message(&msg);
        assert!(
            text.contains("Links:\n  Staff Engineer  https://x.example/1\n  https://x.example/2\n")
        );
        msg.links = Some(vec![]);
        assert!(message(&msg).contains("(none)"));
    }
}
