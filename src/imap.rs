//! The read path: IMAP against Proton Mail Bridge.
//!
//! Bridge serves the account's real folders, so the mailbox names are the ones
//! Proton shows (`INBOX`, `Sent`, `Trash`, `All Mail`, and `Labels/...` or
//! `Folders/...` for the user's own). Two Bridge behaviours are handled here
//! rather than documented away upstream:
//!
//! - SEARCH on content is eventually consistent, so a search that returns
//!   nothing is still a real answer; this module never retries it.
//! - A moved message can carry `\Recent` from the move, so "unread" is read
//!   from `\Seen` and nothing else.

use anyhow::{Context as _, Result, anyhow, bail};
use async_imap::Session;
use async_imap::types::{Flag, Uid};
use futures_util::TryStreamExt as _;
use tokio::net::TcpStream;

use crate::config::Account;
use crate::{FolderRow, MessageRow, address_only};

/// A connected, logged-in session. Plaintext for Bridge on loopback, TLS when
/// the account points at another endpoint.
pub type ImapSession = Session<BoxedStream>;

/// Either a plain TCP stream or a TLS-wrapped one, unified so the session type
/// does not need to be generic through every function here. `Session` requires
/// `Debug`, so the trait carries it; the boxed stream forwards to whatever it
/// wraps.
pub trait Stream:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + std::fmt::Debug
{
}
impl<T> Stream for T where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + std::fmt::Debug
{
}

/// A boxed stream is not `Debug` on its own, so wrap one to keep `Session`
/// bounds satisfied without threading the concrete stream type everywhere.
#[derive(Debug)]
pub struct BoxedStream(Box<dyn Stream>);

impl tokio::io::AsyncRead for BoxedStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for BoxedStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.0).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

pub async fn connect(account: &Account) -> Result<ImapSession> {
    let ep = &account.imap;
    let tcp = TcpStream::connect((ep.host.as_str(), ep.port))
        .await
        .with_context(|| {
            format!(
                "connecting to IMAP at {}:{} (is Proton Bridge running?)",
                ep.host, ep.port
            )
        })?;

    let stream: BoxedStream = if ep.tls {
        let tls = async_native_tls::TlsConnector::new();
        let s = tls
            .connect(&ep.host, tcp)
            .await
            .with_context(|| format!("TLS handshake with {}", ep.host))?;
        BoxedStream(Box::new(s))
    } else {
        BoxedStream(Box::new(tcp))
    };

    let client = async_imap::Client::new(stream);
    // Some servers greet before LOGIN is legal; reading the greeting is what
    // `login` does internally, so nothing extra is needed here.
    let session = client
        .login(&account.user, &account.password)
        .await
        .map_err(|(e, _)| anyhow!("IMAP login failed for {}: {e}", account.user))?;
    Ok(session)
}

pub async fn logout(session: &mut ImapSession) {
    // A logout failure has no recovery and no value to the caller.
    let _ = session.logout().await;
}

/// Every mailbox with its message count. `STATUS` is asked for each one, since
/// `LIST` carries no counts of its own.
pub async fn folders(session: &mut ImapSession) -> Result<Vec<FolderRow>> {
    let names: Vec<String> = session
        .list(Some(""), Some("*"))
        .await
        .context("listing mailboxes")?
        .try_collect::<Vec<_>>()
        .await
        .context("reading the mailbox list")?
        .into_iter()
        .map(|m| m.name().to_string())
        .collect();

    let mut rows = Vec::with_capacity(names.len());
    for name in names {
        match session.status(&name, "(MESSAGES UNSEEN)").await {
            Ok(mb) => rows.push(FolderRow {
                name,
                messages: mb.exists,
                unseen: mb.unseen,
            }),
            // A mailbox that disappears between LIST and STATUS is not an
            // error worth failing the whole listing for.
            Err(_) => rows.push(FolderRow {
                name,
                messages: 0,
                unseen: None,
            }),
        }
    }
    rows.sort_by_key(|a| a.name.to_lowercase());
    Ok(rows)
}

/// Recent messages, newest first.
///
/// `UID SEARCH ALL` then fetch the tail of the result, because IMAP has no
/// "last N" of its own and the UID order is the message order.
pub async fn list(
    session: &mut ImapSession,
    mailbox: &str,
    limit: u32,
    unread: bool,
) -> Result<Vec<MessageRow>> {
    let mb = session
        .select(mailbox)
        .await
        .with_context(|| format!("selecting mailbox {mailbox}"))?;

    if mb.exists == 0 {
        return Ok(Vec::new());
    }

    let filter = if unread { "UNSEEN" } else { "ALL" };
    let mut uids: Vec<Uid> = session
        .uid_search(filter)
        .await
        .with_context(|| format!("searching {mailbox} for {filter}"))?
        .into_iter()
        .collect();
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    // `uid_search` returns a set, so sort before taking the newest.
    sort_uids(&mut uids);
    let start = uids.len().saturating_sub(limit as usize);
    fetch_rows(session, &uids[start..]).await
}

fn sort_uids(uids: &mut [Uid]) {
    uids.sort_unstable();
}

/// Fetch the envelope and flags for a known set of UIDs.
async fn fetch_rows(session: &mut ImapSession, uids: &[Uid]) -> Result<Vec<MessageRow>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let set = uids
        .iter()
        .map(|u| u.to_string())
        .collect::<Vec<_>>()
        .join(",");

    // Headers are fetched raw and parsed with a MIME parser rather than read
    // from IMAP's ENVELOPE, because ENVELOPE returns RFC 2047 encoded words
    // (utf-8?Q?...?=) verbatim. Parsing decodes the subject and the display
    // names, which is what a reader expects to see.
    let fetches: Vec<_> = session
        .uid_fetch(
            set,
            "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (FROM SUBJECT DATE)])",
        )
        .await
        .context("fetching message headers")?
        .try_collect()
        .await
        .context("reading fetched headers")?;

    let mut rows: Vec<MessageRow> = fetches
        .iter()
        .filter_map(|f| {
            let raw = f.header()?;
            let parsed = mail_parser::MessageParser::default().parse(raw)?;
            Some(MessageRow {
                uid: f.uid.unwrap_or(0),
                from: parsed
                    .from()
                    .and_then(|list| {
                        list.iter().find_map(|a| match a {
                            mail_parser::Address::Mailbox(m) => format_address(&m),
                            _ => None,
                        })
                    })
                    .unwrap_or_default(),
                subject: parsed.subject().unwrap_or_default().to_string(),
                date: f
                    .internal_date()
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
                seen: f.flags().any(|fl| matches!(fl, Flag::Seen)),
                flagged: f.flags().any(|fl| matches!(fl, Flag::Flagged)),
            })
        })
        .collect();

    rows.sort_by_key(|r| r.uid);
    Ok(rows)
}

/// `Name <addr>`, or just the address when the display name is absent.
fn format_address(m: &mail_parser::Mailbox<'_>) -> Option<String> {
    let addr = m.address()?;
    match m.name() {
        Some(name) if !name.is_empty() => Some(format!("{name} <{addr}>")),
        _ => Some(addr.to_string()),
    }
}

/// One message in full, with the body decoded and attachments named.
pub async fn read(
    session: &mut ImapSession,
    mailbox: &str,
    uid: Uid,
    body_limit: usize,
) -> Result<crate::Message> {
    session
        .select(mailbox)
        .await
        .with_context(|| format!("selecting mailbox {mailbox}"))?;

    let fetches: Vec<_> = session
        .uid_fetch(
            uid.to_string(),
            "(UID FLAGS ENVELOPE INTERNALDATE BODY.PEEK[])",
        )
        .await
        .with_context(|| format!("fetching message {uid}"))?
        .try_collect()
        .await
        .context("reading the fetched message")?;

    let f = fetches
        .first()
        .ok_or_else(|| anyhow!("no message with UID {uid} in {mailbox}"))?;

    let raw = f
        .body()
        .ok_or_else(|| anyhow!("message {uid} carried no body"))?;
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("message {uid} could not be parsed as MIME"))?;

    let mut body = parsed
        .body_text(0)
        .map(|c| c.to_string())
        .or_else(|| parsed.body_html(0).map(|c| strip_tags(&c)))
        .unwrap_or_default();

    let truncated = body.len() > body_limit;
    if truncated {
        body.truncate(body_limit);
    }

    let attachments = parsed
        .attachments()
        .map(|a| {
            let name = a
                .attachment_name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unnamed".into());
            let ty = a
                .content_type()
                .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
                .unwrap_or_else(|| "application/octet-stream".into());
            format!("{name} ({ty})")
        })
        .collect();

    let env = f.envelope();
    Ok(crate::Message {
        uid,
        mailbox: mailbox.to_string(),
        from: parsed
            .from()
            .map(|list| {
                list.iter()
                    .filter_map(|a| match a {
                        mail_parser::Address::Mailbox(m) => format_address(&m),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        to: parsed
            .to()
            .map(|list| {
                list.iter()
                    .filter_map(|a| match a {
                        mail_parser::Address::Mailbox(m) => format_address(&m),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        cc: parsed
            .cc()
            .map(|list| {
                list.iter()
                    .filter_map(|a| match a {
                        mail_parser::Address::Mailbox(m) => format_address(&m),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        subject: env
            .and_then(|e| e.subject.as_ref())
            .map(|s| String::from_utf8_lossy(s).to_string())
            .unwrap_or_default(),
        date: f
            .internal_date()
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
        message_id: env
            .and_then(|e| e.message_id.as_ref())
            .map(|m| String::from_utf8_lossy(m).to_string()),
        body,
        attachments,
        truncated,
    })
}

/// A crude HTML-to-text pass: enough to read a newsletter, not a renderer.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A search expressed as the terms the caller gave.
pub struct Query {
    pub subject: Option<String>,
    pub from: Option<String>,
    pub body: Option<String>,
    pub since: Option<String>,
}

pub async fn search(
    session: &mut ImapSession,
    mailbox: &str,
    q: &Query,
    limit: u32,
) -> Result<Vec<MessageRow>> {
    session
        .select(mailbox)
        .await
        .with_context(|| format!("selecting mailbox {mailbox}"))?;

    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &q.subject {
        parts.push(format!("SUBJECT {}", imap_string(s)));
    }
    if let Some(f) = &q.from {
        parts.push(format!("FROM {}", imap_string(&address_only(f))));
    }
    if let Some(b) = &q.body {
        parts.push(format!("BODY {}", imap_string(b)));
    }
    if let Some(d) = &q.since {
        parts.push(format!("SINCE {}", imap_date(d)?));
    }
    if parts.is_empty() {
        bail!("give at least one of --subject, --from, --body, --since");
    }

    let mut uids: Vec<Uid> = session
        .uid_search(parts.join(" "))
        .await
        .with_context(|| format!("searching {mailbox}"))?
        .into_iter()
        .collect();
    sort_uids(&mut uids);
    let start = uids.len().saturating_sub(limit as usize);
    fetch_rows(session, &uids[start..]).await
}

/// IMAP strings are quoted; a quote or backslash inside one is escaped.
fn imap_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// IMAP dates are `dd-Mon-yyyy`, and only the 3-letter English month is legal.
fn imap_date(iso: &str) -> Result<String> {
    let d = jiff::civil::Date::strptime("%Y-%m-%d", iso)
        .with_context(|| format!("{iso} is not a YYYY-MM-DD date"))?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(d.month() as usize) - 1];
    Ok(format!("{:02}-{month}-{}", d.day(), d.year()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_date_renders_the_three_letter_month() {
        assert_eq!(imap_date("2026-11-17").unwrap(), "17-Nov-2026");
        assert_eq!(imap_date("2026-01-01").unwrap(), "01-Jan-2026");
        assert_eq!(imap_date("2026-12-31").unwrap(), "31-Dec-2026");
    }

    #[test]
    fn imap_date_rejects_anything_that_is_not_a_date() {
        assert!(imap_date("17-11-2026").is_err());
        assert!(imap_date("tomorrow").is_err());
        assert!(imap_date("2026-02-30").is_err());
    }

    #[test]
    fn imap_string_escapes_quotes_and_backslashes() {
        assert_eq!(imap_string("plain"), "\"plain\"");
        assert_eq!(imap_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(imap_string("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn strip_tags_keeps_the_text_and_drops_the_markup() {
        assert_eq!(strip_tags("<p>hello <b>world</b></p>"), "hello world");
        assert_eq!(strip_tags("a<br/>b"), "a b");
        assert_eq!(strip_tags("no tags"), "no tags");
    }

    #[test]
    fn sort_uids_orders_numerically_not_lexically() {
        let mut v = vec![100, 9, 20];
        sort_uids(&mut v);
        assert_eq!(v, vec![9, 20, 100]);
    }
}
