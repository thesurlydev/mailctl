//! mailctl: read and send mail through Proton Mail Bridge.
//!
//! Bridge is the only supported way to reach Proton Mail from a program on
//! desktop (it decrypts mail locally and exposes plain IMAP and SMTP on
//! loopback), so this tool is a thin, honest client of those two protocols
//! rather than a consumer of any Proton private API.
//!
//! Four commands read (`folders`, `list`, `read`, `search`) and one writes
//! (`send`). Reading is IMAP against Bridge, opened read-only; sending is SMTP
//! submission.
//!
//! The tool is built to be driven by an agent. With `--json`, stdout is always
//! exactly one JSON value (an object or an array), including on failure, where
//! it is `{"error": "...", "kind": "..."}` and the exit code names the kind.
//! Body text is untrusted input from strangers and is returned as data, never
//! executed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use clap::{Parser, Subcommand};
use serde::Serialize;
use tokio::io::AsyncReadExt as _;

mod config;
mod error;
mod imap;
mod model;
mod output;
mod smtp;
mod tls;

use config::Account;
use error::{ErrorBody, Kind, fail, is_broken_pipe, kind_of};

/// Body text is cut at this many characters unless `read --body-limit` says
/// otherwise; the default keeps one `read` from filling a context window.
const DEFAULT_BODY_LIMIT: usize = 20_000;

#[derive(Parser, Debug)]
#[command(
    name = "mailctl",
    version,
    about = "Read and send mail through Proton Mail Bridge",
    after_help = "Exit codes: 0 ok, 1 other, 2 usage, 3 config, 4 Bridge unreachable, \
                  5 login rejected, 6 not found, 7 refused by policy, 8 timeout."
)]
struct Cli {
    /// Print one JSON value on stdout instead of human text, errors included.
    #[arg(long, global = true)]
    json: bool,

    /// Give up after this many seconds (default 30, or MAILCTL_TIMEOUT).
    #[arg(long, global = true, value_name = "SECS")]
    timeout: Option<u64>,

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
    /// Read one message by UID. Reading never marks it as seen.
    Read {
        /// The IMAP UID, as printed by `list`.
        uid: u32,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        /// Cut the body after this many characters; 0 for no limit.
        #[arg(long, default_value_t = DEFAULT_BODY_LIMIT, value_name = "CHARS")]
        body_limit: usize,
    },
    /// Search a mailbox. Terms are combined with AND.
    Search {
        /// Text to match in the subject.
        #[arg(long, allow_hyphen_values = true)]
        subject: Option<String>,
        /// Address to match in the From header.
        #[arg(long)]
        from: Option<String>,
        /// Free text to match in the body.
        #[arg(long, allow_hyphen_values = true)]
        body: Option<String>,
        /// Only messages on or after this date (YYYY-MM-DD).
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value = "INBOX")]
        mailbox: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Send a plain-text message. This cannot be undone; use --dry-run first.
    Send {
        /// Recipient address. Repeat, or separate with commas, for several.
        #[arg(long, required = true)]
        to: Vec<String>,
        #[arg(long)]
        cc: Vec<String>,
        #[arg(long)]
        bcc: Vec<String>,
        /// Subject line. May start with a hyphen.
        #[arg(long, allow_hyphen_values = true)]
        subject: String,
        /// Body text; it may start with a hyphen, as a bullet list does. A
        /// lone `-` reads the body from stdin, which avoids putting a long
        /// body in the process table and keeps quoting out of the way.
        #[arg(long, allow_hyphen_values = true)]
        body: String,
        /// Validate and print the message, check the recipient policy, and
        /// send nothing. Needs no password and no running Bridge.
        #[arg(long)]
        dry_run: bool,
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

/// Set once `send` starts talking to the SMTP server. After that point a
/// timeout cannot say whether the message went out.
static SUBMITTING: AtomicBool = AtomicBool::new(false);

#[tokio::main]
async fn main() {
    let code = real_main().await;
    // Exit explicitly. A read of stdin (`--body -`) runs on a runtime thread
    // that cannot be cancelled, and dropping the runtime would wait for it,
    // so a caller that never closes stdin would hang a command that already
    // reported its timeout.
    std::process::exit(i32::from(code));
}

async fn real_main() -> u8 {
    // clap reports usage errors as text. When the caller asked for JSON, they
    // get the same error object as every other failure instead.
    let wants_json = std::env::args().any(|a| a == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if e.use_stderr() && wants_json => {
            // clap's text starts with its own `error: `; the JSON `kind` says that.
            let rendered = e.to_string();
            let message = rendered.trim().trim_start_matches("error: ").to_string();
            return report(&fail(Kind::Usage, anyhow!(message)), true);
        }
        Err(e) => e.exit(),
    };

    let json = cli.json;
    match run(cli).await {
        Ok(()) => 0,
        Err(err) => report(&err, json),
    }
}

/// Print a failure the way the caller asked for it, and pick the exit code.
fn report(err: &anyhow::Error, json: bool) -> u8 {
    // A reader that closed the pipe early is not an error worth reporting.
    if is_broken_pipe(err) {
        return 0;
    }
    let kind = kind_of(err);
    let message = format!("{err:#}");
    if json {
        let body = ErrorBody {
            error: &message,
            kind: kind.as_str(),
        };
        if let Ok(text) = output::json(&body) {
            let _ = output::emit(&text);
        }
    } else {
        eprintln!("error: {message}");
    }
    kind.exit_code()
}

async fn run(cli: Cli) -> Result<()> {
    let mut account = Account::load()?;
    if let Some(secs) = cli.timeout {
        if secs == 0 {
            return Err(fail(Kind::Usage, anyhow!("--timeout must be at least 1")));
        }
        account.timeout = Duration::from_secs(secs);
    }

    let limit = account.timeout;
    match tokio::time::timeout(limit, dispatch(&account, cli.command, cli.json)).await {
        Ok(result) => result,
        Err(_) => Err(fail(
            Kind::Timeout,
            anyhow!(timeout_message(
                limit.as_secs(),
                SUBMITTING.load(Ordering::Relaxed)
            )),
        )),
    }
}

/// What a timeout says. Once a send has reached the SMTP server the outcome is
/// unknown, and a caller that retries blindly could send the message twice.
fn timeout_message(secs: u64, submitting: bool) -> String {
    if submitting {
        format!(
            "timed out after {secs}s while sending: the message may or may not have been \
             delivered. Check the Sent folder before trying again (raise the limit with \
             --timeout or MAILCTL_TIMEOUT)"
        )
    } else {
        format!(
            "timed out after {secs}s (is Proton Bridge responsive? raise it with \
             --timeout or MAILCTL_TIMEOUT)"
        )
    }
}

/// Run `f` on a logged-in session and always log out, whether it worked or not.
async fn with_imap<T>(
    account: &Account,
    f: impl AsyncFnOnce(&mut imap::ImapSession) -> Result<T>,
) -> Result<T> {
    let mut session = imap::connect(account).await?;
    let result = f(&mut session).await;
    imap::logout(&mut session).await;
    result
}

async fn dispatch(account: &Account, command: Command, json: bool) -> Result<()> {
    let text = match command {
        Command::Whoami => whoami(account, json)?,
        Command::Folders => {
            let rows = with_imap(account, async |s| imap::folders(s).await).await?;
            if json {
                output::json(&rows)?
            } else {
                output::folders(&rows)
            }
        }
        Command::List {
            mailbox,
            limit,
            filter,
        } => {
            let unread = filter == Filter::Unread;
            let rows = with_imap(account, async |s| {
                imap::list(s, &mailbox, limit, unread).await
            })
            .await?;
            if json {
                output::json(&rows)?
            } else {
                output::message_rows(&rows)
            }
        }
        Command::Read {
            uid,
            mailbox,
            body_limit,
        } => {
            let msg = with_imap(account, async |s| {
                imap::read(s, &mailbox, uid, body_limit).await
            })
            .await?;
            if json {
                output::json(&msg)?
            } else {
                output::message(&msg)
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
            // Validate the terms first: a usage error needs no password and no
            // connection.
            let criteria = query.to_criteria()?;
            let rows = with_imap(account, async |s| {
                imap::search(s, &mailbox, &criteria, limit).await
            })
            .await?;
            if json {
                output::json(&rows)?
            } else {
                output::message_rows(&rows)
            }
        }
        Command::Send {
            to,
            cc,
            bcc,
            subject,
            body,
            dry_run,
        } => send(account, to, cc, bcc, subject, body, dry_run, json).await?,
    };
    output::emit(&text).context("writing output")?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send(
    account: &Account,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body: String,
    dry_run: bool,
    json: bool,
) -> Result<String> {
    // A read-only account refuses before anything else, so it does not even
    // consume stdin first.
    account.policy.check_enabled()?;

    let body = if body == "-" {
        let mut s = String::new();
        tokio::io::stdin()
            .read_to_string(&mut s)
            .await
            .context("reading the body from stdin")?;
        s
    } else {
        body
    };

    let draft = smtp::build(account, &to, &cc, &bcc, &subject, &body)?;
    // The policy is checked on a dry run too, so a caller finds out before it
    // asks for approval to send something that would be refused.
    account.policy.check_send(&draft.recipients())?;

    if dry_run {
        return preview(&draft, json);
    }

    let message_id = draft.message_id.clone();
    SUBMITTING.store(true, Ordering::Relaxed);
    smtp::send(account, draft).await?;
    if json {
        output::json(&serde_json::json!({ "sent": true, "message_id": message_id }))
    } else {
        Ok(format!("sent (message-id {message_id})"))
    }
}

#[derive(Serialize)]
struct Preview<'a> {
    sent: bool,
    dry_run: bool,
    /// A real send generates a new id; this one only shows the shape.
    message_id: &'a str,
    from: &'a str,
    to: &'a [String],
    cc: &'a [String],
    bcc: &'a [String],
    subject: &'a str,
    body: &'a str,
}

fn preview(draft: &smtp::Draft, json: bool) -> Result<String> {
    if json {
        return output::json(&Preview {
            sent: false,
            dry_run: true,
            message_id: &draft.message_id,
            from: &draft.from,
            to: &draft.to,
            cc: &draft.cc,
            bcc: &draft.bcc,
            subject: &draft.subject,
            body: &draft.body,
        });
    }
    let mut s = String::from("dry run: nothing was sent\n");
    s.push_str(&format!("From:    {}\n", draft.from));
    s.push_str(&format!("To:      {}\n", draft.to.join(", ")));
    if !draft.cc.is_empty() {
        s.push_str(&format!("Cc:      {}\n", draft.cc.join(", ")));
    }
    if !draft.bcc.is_empty() {
        s.push_str(&format!("Bcc:     {}\n", draft.bcc.join(", ")));
    }
    s.push_str(&format!(
        "Subject: {}\n{}\n{}\n",
        draft.subject,
        "-".repeat(60),
        draft.body
    ));
    Ok(s)
}

fn whoami(account: &Account, json: bool) -> Result<String> {
    if json {
        let view = serde_json::json!({
            "user": account.user,
            "imap": {
                "host": account.imap.host,
                "port": account.imap.port,
                "tls": account.imap.tls,
                "ca_cert": account.imap.ca_cert,
            },
            "smtp": {
                "host": account.smtp.host,
                "port": account.smtp.port,
                "tls": account.smtp.tls,
                "starttls": account.smtp_starttls,
                "ca_cert": account.smtp.ca_cert,
            },
            "read_only": account.policy.read_only,
            "allowed_recipients": account.policy.allowed_recipients,
            "timeout_secs": account.timeout.as_secs(),
            "config": account.config_path,
            "password": "<redacted>",
        });
        output::json(&view)
    } else {
        let mut s = format!("user: {}\n", account.user);
        s.push_str(&format!(
            "imap: {}:{}\n",
            account.imap.host, account.imap.port
        ));
        s.push_str(&format!(
            "smtp: {}:{}\n",
            account.smtp.host, account.smtp.port
        ));
        s.push_str(&format!("read_only: {}\n", account.policy.read_only));
        if !account.policy.allowed_recipients.is_empty() {
            s.push_str(&format!(
                "allowed_recipients: {}\n",
                account.policy.allowed_recipients.join(", ")
            ));
        }
        s.push_str(&format!("config: {}\n", account.config_path.display()));
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_while_sending_says_the_outcome_is_unknown() {
        let sending = timeout_message(30, true);
        assert!(sending.contains("may or may not"), "{sending}");
        assert!(sending.contains("Sent folder"), "{sending}");
        let reading = timeout_message(30, false);
        assert!(!reading.contains("may or may not"), "{reading}");
        assert!(reading.contains("30s"), "{reading}");
    }
}
