//! The read path: IMAP against Proton Mail Bridge.
//!
//! Bridge serves the account's real folders, so the mailbox names are the ones
//! Proton shows (`INBOX`, `Sent`, `Trash`, `All Mail`, and `Labels/...` or
//! `Folders/...` for the user's own). Behaviours handled here rather than
//! documented away:
//!
//! - Every mailbox is opened with `EXAMINE`, which is read-only by protocol, so
//!   no command in this module can change a flag or move a message. Bodies are
//!   fetched with `BODY.PEEK`, so reading never marks a message as seen.
//! - SEARCH on content is eventually consistent, so a search that returns
//!   nothing is still a real answer; this module never retries it.
//! - A moved message can carry `\Recent` from the move, so "unread" is read
//!   from `\Seen` and nothing else.

use anyhow::{Context as _, Result, anyhow};
use async_imap::Session;
use async_imap::error::Error as ImapError;
use async_imap::types::{Flag, Uid};
use futures_util::TryStreamExt as _;
use tokio::net::TcpStream;

use crate::config::Account;
use crate::error::{Kind, fail};
use crate::model::{FolderRow, Message, MessageRow, addr_list, address_only, truncate_chars};
use crate::tls;

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
    let password = account.password().await?;
    let ep = &account.imap;
    let tcp = TcpStream::connect((ep.host.as_str(), ep.port))
        .await
        .map_err(|e| {
            fail(
                Kind::Unreachable,
                anyhow::Error::new(e).context(format!(
                    "connecting to IMAP at {}:{} (is Proton Bridge running?)",
                    ep.host, ep.port
                )),
            )
        })?;

    let stream: BoxedStream = if ep.tls {
        let connector = tls::connector(ep.ca_cert.as_deref()).map_err(|e| fail(Kind::Config, e))?;
        let name = tls::server_name(&ep.host).map_err(|e| fail(Kind::Config, e))?;
        let s = connector
            .connect(name, tcp)
            .await
            .with_context(|| format!("TLS handshake with {}", ep.host))?;
        BoxedStream(Box::new(s))
    } else {
        BoxedStream(Box::new(tcp))
    };

    let client = async_imap::Client::new(stream);
    let session = client
        .login(&account.user, password.expose())
        .await
        .map_err(|(e, _)| {
            fail(
                Kind::Auth,
                anyhow!(
                    "IMAP login failed for {} (the Bridge password is not the Proton account password): {e}",
                    account.user
                ),
            )
        })?;
    Ok(session)
}

pub async fn logout(session: &mut ImapSession) {
    // A logout failure has no recovery and no value to the caller.
    let _ = session.logout().await;
}

/// Open a mailbox read-only. A refusal from the server means it is not there.
async fn open(session: &mut ImapSession, mailbox: &str) -> Result<async_imap::types::Mailbox> {
    session.examine(mailbox).await.map_err(|e| match e {
        ImapError::No(msg) => fail(
            Kind::NotFound,
            anyhow!("no mailbox named {mailbox} (try `mailctl folders`): {msg}"),
        ),
        other => anyhow::Error::new(other).context(format!("opening mailbox {mailbox}")),
    })
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
    let mb = open(session, mailbox).await?;

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
    uids.sort_unstable();
    let start = uids.len().saturating_sub(limit as usize);
    fetch_rows(session, &uids[start..]).await
}

/// Fetch the headers and flags for a known set of UIDs, newest first.
async fn fetch_rows(session: &mut ImapSession, uids: &[Uid]) -> Result<Vec<MessageRow>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let set = uid_set(uids);

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
            let row = parse_row(
                f.header()?,
                f.uid.unwrap_or(0),
                f.internal_date()
                    .map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
                f.flags().any(|fl| matches!(fl, Flag::Seen)),
                f.flags().any(|fl| matches!(fl, Flag::Flagged)),
            )?;
            Some(row)
        })
        .collect();

    // Newest first, as documented.
    rows.sort_by_key(|r| std::cmp::Reverse(r.uid));
    Ok(rows)
}

/// UIDs as an IMAP sequence set, with runs collapsed (`3,4,5,9` becomes
/// `3:5,9`) so a large `--limit` does not build a command line of thousands of
/// numbers. The input must be sorted and without duplicates.
fn uid_set(uids: &[Uid]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < uids.len() {
        let start = uids[i];
        let mut end = start;
        while i + 1 < uids.len() && uids[i + 1] == end + 1 {
            i += 1;
            end = uids[i];
        }
        parts.push(if end == start {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        i += 1;
    }
    parts.join(",")
}

/// One list row from raw header bytes.
fn parse_row(
    raw: &[u8],
    uid: u32,
    date: Option<String>,
    seen: bool,
    flagged: bool,
) -> Option<MessageRow> {
    let parsed = mail_parser::MessageParser::default().parse(raw)?;
    Some(MessageRow {
        uid,
        from: addr_list(parsed.from()),
        subject: parsed.subject().unwrap_or_default().to_string(),
        date,
        seen,
        flagged,
    })
}

/// One message in full, with the body decoded and attachments named. The body
/// is cut at `body_limit` characters (0 for no limit). With `with_links`, the
/// links of the HTML part come back too, since the text body drops them.
pub async fn read(
    session: &mut ImapSession,
    mailbox: &str,
    uid: Uid,
    body_limit: usize,
    with_links: bool,
) -> Result<Message> {
    open(session, mailbox).await?;

    let fetches: Vec<_> = session
        .uid_fetch(uid.to_string(), "(UID FLAGS INTERNALDATE BODY.PEEK[])")
        .await
        .with_context(|| format!("fetching message {uid}"))?
        .try_collect()
        .await
        .context("reading the fetched message")?;

    let f = fetches.first().ok_or_else(|| {
        fail(
            Kind::NotFound,
            anyhow!("no message with UID {uid} in {mailbox}"),
        )
    })?;

    let raw = f
        .body()
        .ok_or_else(|| anyhow!("message {uid} carried no body"))?;
    let mut msg = parse_message(
        raw,
        uid,
        mailbox,
        f.internal_date()
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
        body_limit,
    )?;
    if with_links {
        msg.links = Some(crate::links::from_raw(raw));
    }
    Ok(msg)
}

/// Decode a raw RFC 5322 message. Pure, so the decoding rules are testable
/// without a mail server.
pub fn parse_message(
    raw: &[u8],
    uid: Uid,
    mailbox: &str,
    date: Option<String>,
    body_limit: usize,
) -> Result<Message> {
    let parsed = mail_parser::MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow!("message {uid} could not be parsed as MIME"))?;

    // `body_text` also renders an HTML-only message to text.
    let body = parsed
        .body_text(0)
        .map(|c| c.to_string())
        .unwrap_or_default();
    let (body, truncated) = truncate_chars(body, body_limit);

    let attachments = parsed
        .attachments()
        .map(|a| {
            let name = a.attachment_name().unwrap_or("unnamed");
            let ty = a
                .content_type()
                .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("octet-stream")))
                .unwrap_or_else(|| "application/octet-stream".into());
            format!("{name} ({ty})")
        })
        .collect();

    Ok(Message {
        uid,
        mailbox: mailbox.to_string(),
        from: addr_list(parsed.from()),
        to: addr_list(parsed.to()),
        cc: addr_list(parsed.cc()),
        // Decoded here, from the header itself, like `list` and `search`.
        subject: parsed.subject().unwrap_or_default().to_string(),
        date,
        // Kept in the `<id@host>` form `send` reports, so one can be searched with the other.
        message_id: parsed.message_id().map(|m| format!("<{m}>")),
        body,
        attachments,
        truncated,
        links: None,
    })
}

/// A search expressed as the terms the caller gave.
pub struct Query {
    pub subject: Option<String>,
    pub from: Option<String>,
    pub body: Option<String>,
    pub since: Option<String>,
}

/// A search term that was actually given. An empty or blank value counts as
/// absent, because a harness that fills every parameter of a fixed command
/// template passes `--from ''` for the ones the caller left out.
fn given(term: &Option<String>) -> Option<&str> {
    term.as_deref().filter(|s| !s.trim().is_empty())
}

impl Query {
    /// The IMAP SEARCH criteria for these terms, ANDed together.
    pub fn to_criteria(&self) -> Result<String> {
        let mut parts: Vec<String> = Vec::new();
        if let Some(s) = given(&self.subject) {
            parts.push(format!("SUBJECT {}", imap_string(s)));
        }
        if let Some(f) = given(&self.from) {
            parts.push(format!("FROM {}", imap_string(&address_only(f))));
        }
        if let Some(b) = given(&self.body) {
            parts.push(format!("BODY {}", imap_string(b)));
        }
        if let Some(d) = given(&self.since) {
            parts.push(format!("SINCE {}", imap_date(d)?));
        }
        if parts.is_empty() {
            return Err(fail(
                Kind::Usage,
                anyhow!("give at least one of --subject, --from, --body, --since"),
            ));
        }
        Ok(parts.join(" "))
    }
}

/// Run a search. `criteria` comes from [`Query::to_criteria`], which the caller
/// runs first so a bad query is reported before anything connects.
pub async fn search(
    session: &mut ImapSession,
    mailbox: &str,
    criteria: &str,
    limit: u32,
) -> Result<Vec<MessageRow>> {
    open(session, mailbox).await?;

    let mut uids: Vec<Uid> = session
        .uid_search(criteria)
        .await
        .with_context(|| format!("searching {mailbox}"))?
        .into_iter()
        .collect();
    uids.sort_unstable();
    let start = uids.len().saturating_sub(limit as usize);
    fetch_rows(session, &uids[start..]).await
}

/// IMAP strings are quoted; a quote or backslash inside one is escaped. A
/// bare CR or LF cannot appear in a quoted string at all, and one would let a
/// caller smuggle a second command into the line, so they are refused upstream
/// by being dropped here.
fn imap_string(s: &str) -> String {
    let escaped = s
        .replace(['\r', '\n'], " ")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// IMAP dates are `dd-Mon-yyyy`, and only the 3-letter English month is legal.
fn imap_date(iso: &str) -> Result<String> {
    let d = jiff::civil::Date::strptime("%Y-%m-%d", iso)
        .map_err(|e| fail(Kind::Usage, anyhow!("{iso} is not a YYYY-MM-DD date: {e}")))?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(d.month() as usize) - 1];
    Ok(format!("{:02}-{month}-{}", d.day(), d.year()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::kind_of;

    #[test]
    fn imap_date_renders_the_three_letter_month() {
        assert_eq!(imap_date("2026-11-17").unwrap(), "17-Nov-2026");
        assert_eq!(imap_date("2026-01-01").unwrap(), "01-Jan-2026");
        assert_eq!(imap_date("2026-12-31").unwrap(), "31-Dec-2026");
    }

    #[test]
    fn imap_date_rejects_anything_that_is_not_a_date() {
        for bad in ["17-11-2026", "tomorrow", "2026-02-30"] {
            let err = imap_date(bad).unwrap_err();
            assert_eq!(kind_of(&err), Kind::Usage, "{bad}");
        }
    }

    #[test]
    fn imap_string_escapes_quotes_and_backslashes() {
        assert_eq!(imap_string("plain"), "\"plain\"");
        assert_eq!(imap_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(imap_string("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn imap_string_cannot_carry_a_line_break() {
        let s = imap_string("a\r\nA1 DELETE INBOX");
        assert!(!s.contains('\r') && !s.contains('\n'), "{s:?}");
    }

    #[test]
    fn query_criteria_and_terms_together() {
        let q = Query {
            subject: Some("invoice".into()),
            from: Some("Ada <ada@example.com>".into()),
            body: None,
            since: Some("2026-11-01".into()),
        };
        assert_eq!(
            q.to_criteria().unwrap(),
            "SUBJECT \"invoice\" FROM \"ada@example.com\" SINCE 01-Nov-2026"
        );
    }

    #[test]
    fn empty_and_blank_terms_count_as_absent() {
        let q = Query {
            subject: Some(String::new()),
            from: Some("ada@example.com".into()),
            body: Some("   ".into()),
            since: Some(String::new()),
        };
        assert_eq!(q.to_criteria().unwrap(), "FROM \"ada@example.com\"");
        let all_blank = Query {
            subject: Some(String::new()),
            from: None,
            body: Some(String::new()),
            since: Some(String::new()),
        };
        assert_eq!(kind_of(&all_blank.to_criteria().unwrap_err()), Kind::Usage);
    }

    #[test]
    fn an_empty_query_is_a_usage_error() {
        let q = Query {
            subject: None,
            from: None,
            body: None,
            since: None,
        };
        assert_eq!(kind_of(&q.to_criteria().unwrap_err()), Kind::Usage);
    }

    const PLAIN: &[u8] = b"From: =?UTF-8?Q?Ren=C3=A9e_Dupont?= <renee@example.com>\r\n\
To: Me <me@example.com>, \"Other\" <other@example.com>\r\n\
Cc: cc@example.com\r\n\
Subject: =?UTF-8?B?Q2Fmw6kgbWVudQ==?=\r\n\
Message-ID: <abc123@example.com>\r\n\
Date: Tue, 17 Nov 2026 09:30:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hello there.\r\n";

    #[test]
    fn read_decodes_the_subject_and_names_like_list_does() {
        let msg =
            parse_message(PLAIN, 42, "INBOX", Some("2026-11-17 09:30".into()), 20_000).unwrap();
        assert_eq!(msg.subject, "Caf\u{e9} menu");
        assert_eq!(msg.from, "Ren\u{e9}e Dupont <renee@example.com>");
        assert_eq!(msg.to, "Me <me@example.com>, Other <other@example.com>");
        assert_eq!(msg.cc, "cc@example.com");
        assert_eq!(msg.message_id.as_deref(), Some("<abc123@example.com>"));
        assert_eq!(msg.body.trim(), "Hello there.");
        assert!(!msg.truncated);
        assert_eq!((msg.uid, msg.mailbox.as_str()), (42, "INBOX"));

        // The same header through the list path must decode identically.
        let header = &PLAIN[..PLAIN.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4];
        let row = parse_row(header, 42, None, true, false).unwrap();
        assert_eq!(row.subject, msg.subject);
        assert_eq!(row.from, msg.from);
    }

    #[test]
    fn a_long_non_ascii_body_is_cut_without_panicking() {
        let mut raw = b"Subject: long\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n".to_vec();
        raw.extend("\u{20ac}".repeat(30_000).as_bytes());
        let msg = parse_message(&raw, 1, "INBOX", None, 20_000).unwrap();
        assert!(msg.truncated);
        assert_eq!(msg.body.chars().count(), 20_000);
    }

    #[test]
    fn a_zero_body_limit_returns_the_whole_body() {
        let mut raw = b"Subject: long\r\nContent-Type: text/plain\r\n\r\n".to_vec();
        raw.extend("x".repeat(50_000).as_bytes());
        let msg = parse_message(&raw, 1, "INBOX", None, 0).unwrap();
        assert!(!msg.truncated);
        assert_eq!(msg.body.len(), 50_000);
    }

    #[test]
    fn an_html_only_message_is_rendered_to_text() {
        let raw = b"Subject: news\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
<html><head><style>p { color: red }</style></head><body><p>Hello <b>world</b> &amp; friends</p></body></html>\r\n";
        let msg = parse_message(raw, 1, "INBOX", None, 20_000).unwrap();
        assert!(msg.body.contains("Hello"), "{:?}", msg.body);
        assert!(msg.body.contains("world"), "{:?}", msg.body);
        assert!(!msg.body.contains("<b>"), "{:?}", msg.body);
        assert!(!msg.body.contains("color: red"), "{:?}", msg.body);
        assert!(msg.body.contains("& friends"), "{:?}", msg.body);
    }

    #[test]
    fn attachments_are_named_with_their_type() {
        let raw = b"Subject: files\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
--b\r\nContent-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\r\nJVBERi0=\r\n\
--b--\r\n";
        let msg = parse_message(raw, 1, "INBOX", None, 20_000).unwrap();
        assert_eq!(
            msg.attachments,
            vec!["report.pdf (application/pdf)".to_string()]
        );
        assert_eq!(msg.body.trim(), "See attached.");
    }

    #[test]
    fn a_group_address_is_flattened() {
        let raw = b"From: a@example.com\r\nTo: team: x@example.com, Y <y@example.com>;\r\nSubject: g\r\n\r\nhi\r\n";
        let msg = parse_message(raw, 1, "INBOX", None, 20_000).unwrap();
        assert_eq!(msg.to, "x@example.com, Y <y@example.com>");
    }

    #[test]
    fn uid_set_collapses_runs() {
        assert_eq!(uid_set(&[]), "");
        assert_eq!(uid_set(&[7]), "7");
        assert_eq!(uid_set(&[3, 4, 5, 9]), "3:5,9");
        assert_eq!(uid_set(&[1, 3, 5]), "1,3,5");
        assert_eq!(uid_set(&[1, 2, 10, 11, 12, 20]), "1:2,10:12,20");
        let big: Vec<Uid> = (1..=5000).collect();
        assert_eq!(uid_set(&big), "1:5000");
    }
}
