//! mailctl: read and send mail through Proton Mail Bridge.
//!
//! Bridge is the only supported way to reach Proton Mail from a program on
//! desktop (it decrypts mail locally and exposes plain IMAP and SMTP on
//! loopback), so this tool is a thin, honest client of those two protocols
//! rather than a consumer of any Proton private API.
//!
//! Two commands read (`folders`, `list`, `read`, `search`) and one writes
//! (`send`). Reading is IMAP against Bridge; sending is SMTP submission, which
//! may go through Bridge on loopback or straight to Proton's submission
//! endpoint, since both accept the account's credentials.
//!
//! The tool is built to be driven by an agent: every command takes `--json`
//! and prints one JSON object, so a caller never parses human prose. Body text
//! is untrusted input from strangers and is returned as data, never executed.

use std::io::Write as _;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

mod config;
mod imap;
mod smtp;

use config::Account;

#[derive(Parser, Debug)]
#[command(
    name = "mailctl",
    version,
    about = "Read and send mail through Proton Mail Bridge"
)]
struct Cli {
    /// Print one JSON object instead of human text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List mailboxes with message counts.
    Folders,
    /// List recent messages in a mailbox, newest first.
    List {
        /// Mailbox to list. Bridge names the inbox `INBOX`.
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        /// How many messages to show.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Which messages: everything, or only unread ones.
        #[arg(long, value_enum, default_value_t = Filter::All)]
        filter: Filter,
    },
    /// Read one message by UID.
    Read {
        /// The IMAP UID, as printed by `list`.
        uid: u32,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
    },
    /// Search a mailbox. Terms are combined with AND.
    Search {
        /// Text to match in the subject.
        #[arg(long)]
        subject: Option<String>,
        /// Address to match in the From header.
        #[arg(long)]
        from: Option<String>,
        /// Free text to match in the body.
        #[arg(long)]
        body: Option<String>,
        /// Only messages on or after this date (YYYY-MM-DD).
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Send a message.
    Send {
        /// Recipient address. Repeat for several.
        #[arg(long, required = true)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        bcc: Vec<String>,
        #[arg(long)]
        subject: String,
        /// Body text. `-` reads it from stdin, which avoids putting a long
        /// body in the process table and keeps quoting out of the way.
        #[arg(long)]
        body: String,
    },
    /// Print the resolved account settings, with the password redacted.
    Whoami,
}

/// Which messages a listing includes.
#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
enum Filter {
    All,
    Unread,
}

#[derive(Serialize)]
struct FolderRow {
    name: String,
    messages: u32,
    unseen: Option<u32>,
}

#[derive(Serialize)]
struct MessageRow {
    uid: u32,
    from: String,
    subject: String,
    date: Option<String>,
    seen: bool,
    flagged: bool,
}

#[derive(Serialize)]
struct Message {
    uid: u32,
    mailbox: String,
    from: String,
    to: String,
    cc: String,
    subject: String,
    date: Option<String>,
    message_id: Option<String>,
    body: String,
    attachments: Vec<String>,
    truncated: bool,
}

/// Body text is cut here. A caller that needs the whole body can raise it with
/// `--body-limit`; the default keeps one `read` from filling a context window.
const DEFAULT_BODY_LIMIT: usize = 20_000;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let account = Account::load().context("loading account settings")?;

    match cli.command {
        Command::Whoami => whoami(&account, cli.json),
        Command::Folders => {
            let mut session = imap::connect(&account).await?;
            let rows = imap::folders(&mut session).await?;
            imap::logout(&mut session).await;
            if cli.json {
                print_json(&rows)
            } else {
                for r in &rows {
                    println!(
                        "{:<40} {:>7}{}",
                        r.name,
                        r.messages,
                        r.unseen
                            .filter(|u| *u > 0)
                            .map(|u| format!("  {u} unread"))
                            .unwrap_or_default()
                    );
                }
                Ok(())
            }
        }
        Command::List {
            mailbox,
            limit,
            filter,
        } => {
            let mut session = imap::connect(&account).await?;
            let rows = imap::list(&mut session, &mailbox, limit, filter == Filter::Unread).await?;
            imap::logout(&mut session).await;
            if cli.json {
                print_json(&rows)
            } else {
                for r in &rows {
                    println!(
                        "{:>6}  {:<28}  {:<38}  {}",
                        r.uid,
                        truncate(&r.from, 28),
                        truncate(&r.subject, 38),
                        r.date.clone().unwrap_or_default()
                    );
                }
                Ok(())
            }
        }
        Command::Read { uid, mailbox } => {
            let mut session = imap::connect(&account).await?;
            let msg = imap::read(&mut session, &mailbox, uid, DEFAULT_BODY_LIMIT).await?;
            imap::logout(&mut session).await;
            if cli.json {
                print_json(&msg)
            } else {
                print_message(&msg);
                Ok(())
            }
        }
        Command::Search {
            subject,
            from,
            body,
            since,
            mailbox,
            limit,
        } => {
            let query = imap::Query {
                subject,
                from,
                body,
                since,
            };
            let mut session = imap::connect(&account).await?;
            let rows = imap::search(&mut session, &mailbox, &query, limit).await?;
            imap::logout(&mut session).await;
            if cli.json {
                print_json(&rows)
            } else {
                for r in &rows {
                    println!(
                        "{:>6}  {:<28}  {:<38}  {}",
                        r.uid,
                        truncate(&r.from, 28),
                        truncate(&r.subject, 38),
                        r.date.clone().unwrap_or_default()
                    );
                }
                Ok(())
            }
        }
        Command::Send {
            to,
            cc,
            bcc,
            subject,
            body,
        } => {
            let body = if body == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                    .context("reading the body from stdin")?;
                s
            } else {
                body
            };
            let out = smtp::send(&account, &to, &cc, &bcc, &subject, &body).await?;
            if cli.json {
                print_json(&serde_json::json!({ "sent": true, "message_id": out }))
            } else {
                println!("sent (message-id {out})");
                Ok(())
            }
        }
    }
}

fn whoami(account: &Account, json: bool) -> Result<()> {
    if json {
        let view = serde_json::json!({
            "user": account.user,
            "imap": { "host": account.imap.host, "port": account.imap.port },
            "smtp": { "host": account.smtp.host, "port": account.smtp.port },
            "password": "<redacted>",
        });
        print_json(&view)
    } else {
        println!("user: {}", account.user);
        println!("imap: {}:{}", account.imap.host, account.imap.port);
        println!("smtp: {}:{}", account.smtp.host, account.smtp.port);
        Ok(())
    }
}

fn print_message(msg: &Message) {
    println!("From:    {}", msg.from);
    println!("To:      {}", msg.to);
    if !msg.cc.is_empty() {
        println!("Cc:      {}", msg.cc);
    }
    println!("Subject: {}", msg.subject);
    if let Some(d) = &msg.date {
        println!("Date:    {d}");
    }
    if !msg.attachments.is_empty() {
        println!("Files:   {}", msg.attachments.join(", "));
    }
    println!("{}", "-".repeat(60));
    println!("{}", msg.body);
    if msg.truncated {
        println!("\n[body truncated]");
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one = s.replace(['\n', '\r'], " ");
    let mut out: String = one.chars().take(n).collect();
    if one.chars().count() > n {
        out.push('…');
    }
    out
}

fn print_json<T: Serialize>(v: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(v)?;
    writeln!(std::io::stdout(), "{s}")?;
    Ok(())
}

/// Split a `Name <addr>` header into the address, which is what a caller
/// usually wants to match on.
pub fn address_only(s: &str) -> String {
    match (s.rfind('<'), s.rfind('>')) {
        (Some(a), Some(b)) if b > a => s[a + 1..b].trim().to_string(),
        _ => s.trim().to_string(),
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
    fn truncate_collapses_newlines_and_marks_the_cut() {
        assert_eq!(truncate("a\nb", 10), "a b");
        let long = "abcdefghij";
        assert_eq!(truncate(long, 4), "abcd\u{2026}");
        assert_eq!(truncate("abc", 10), "abc");
    }
}
