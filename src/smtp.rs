//! The write path: SMTP submission.
//!
//! Sending is split in two so it can be previewed: [`build`] validates the
//! recipients and assembles the message with no network and no password, and
//! [`send`] submits it. `send --dry-run` stops after `build`. The message id
//! comes back, since that is the only proof a caller can keep that a message
//! really left.

use anyhow::{Context as _, Result, anyhow};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Certificate, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as SmtpMessage, Tokio1Executor};

use crate::config::{Account, Endpoint};
use crate::error::{Kind, fail};

/// A validated message, ready to preview or to submit.
pub struct Draft {
    pub message: SmtpMessage,
    pub message_id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

impl Draft {
    /// Every address the message will be delivered to, which is what the
    /// recipient policy checks. Bcc counts: it is delivered even though no
    /// header shows it.
    pub fn recipients(&self) -> Vec<String> {
        self.to
            .iter()
            .chain(&self.cc)
            .chain(&self.bcc)
            .cloned()
            .collect()
    }
}

/// A fresh `<random@domain>` Message-ID on the sender's own domain. lettre's
/// default would use this machine's hostname, which then ships in the headers
/// of every message sent.
fn new_message_id(domain: &str) -> String {
    use std::hash::{BuildHasher as _, Hasher as _};
    use std::time::{SystemTime, UNIX_EPOCH};

    // `RandomState` is seeded randomly per instance, so no extra dependency is
    // needed for 128 unpredictable bits.
    let part = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        h.write_u128(nanos);
        h.finish()
    };
    format!("<{:016x}{:016x}@{domain}>", part(), part())
}

/// Split addresses on commas and trim, so `a@x, b@y` is two recipients. A comma
/// inside a quoted display name or an `<address>` does not split, so
/// `"Doe, John" <j@x.com>` stays one recipient.
fn split(addrs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for text in addrs {
        let mut current = String::new();
        let (mut quoted, mut escaped, mut angle) = (false, false, false);
        for ch in text.chars() {
            if escaped {
                escaped = false;
                current.push(ch);
                continue;
            }
            match ch {
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                '<' if !quoted => angle = true,
                '>' if !quoted => angle = false,
                ',' if !quoted && !angle => {
                    out.push(std::mem::take(&mut current));
                    continue;
                }
                _ => {}
            }
            current.push(ch);
        }
        out.push(current);
    }
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_recipients(role: &str, addrs: &[String]) -> Result<Vec<Mailbox>> {
    split(addrs)
        .iter()
        .map(|addr| {
            addr.parse::<Mailbox>().map_err(|e| {
                fail(
                    Kind::Usage,
                    anyhow!("{addr} is not a valid {role} address: {e}"),
                )
            })
        })
        .collect()
}

/// Assemble one plain-text message. Nothing here touches the network.
pub fn build(
    account: &Account,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
) -> Result<Draft> {
    let from: Mailbox = account.user.parse().map_err(|e| {
        fail(
            Kind::Config,
            anyhow!("{} is not a usable From address: {e}", account.user),
        )
    })?;
    let to = parse_recipients("To", to)?;
    let cc = parse_recipients("Cc", cc)?;
    let bcc = parse_recipients("Bcc", bcc)?;
    if to.is_empty() {
        return Err(fail(Kind::Usage, anyhow!("give at least one --to address")));
    }

    // A Message-ID is generated locally rather than left to the relay, because
    // it is what `send` reports back and what a later search uses to find the
    // message.
    let mut builder = SmtpMessage::builder()
        .from(from.clone())
        .subject(subject)
        .message_id(Some(new_message_id(from.email.domain())));
    for m in &to {
        builder = builder.to(m.clone());
    }
    for m in &cc {
        builder = builder.cc(m.clone());
    }
    // lettre delivers to Bcc recipients but drops the header, so the other
    // recipients never see them; a test pins that.
    for m in &bcc {
        builder = builder.bcc(m.clone());
    }

    // A plain-text body is what Proton and every mail client handle most
    // predictably, and it keeps an agent from emitting accidental HTML.
    let message = builder
        .body(body.to_string())
        .map_err(|e| fail(Kind::Usage, anyhow!("building the message: {e}")))?;
    let message_id = message
        .headers()
        .get_raw("Message-ID")
        .map(|v| v.trim().to_string())
        .unwrap_or_default();

    let addrs = |list: &[Mailbox]| list.iter().map(|m| m.email.to_string()).collect();
    Ok(Draft {
        message,
        message_id,
        from: from.email.to_string(),
        to: addrs(&to),
        cc: addrs(&cc),
        bcc: addrs(&bcc),
        subject: subject.to_string(),
        body: body.to_string(),
    })
}

/// TLS parameters for an endpoint: the bundled roots plus its `ca_cert`.
fn tls_parameters(ep: &Endpoint) -> Result<TlsParameters> {
    let mut builder = TlsParameters::builder(ep.host.clone());
    if let Some(path) = &ep.ca_cert {
        let pem =
            std::fs::read(path).with_context(|| format!("reading ca_cert {}", path.display()))?;
        let cert = Certificate::from_pem(&pem)
            .with_context(|| format!("parsing ca_cert {}", path.display()))?;
        builder = builder.add_root_certificate(cert);
    }
    builder.build_rustls().context("configuring SMTP TLS")
}

/// Submit a built message. Returns once the server has accepted it.
pub async fn send(account: &Account, draft: Draft) -> Result<()> {
    let password = account.password().await?;
    let ep = &account.smtp;
    let creds = Credentials::new(account.user.clone(), password.expose().to_string());

    // The builder starts with no TLS; each mode below adds what it needs.
    let builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&ep.host)
        .port(ep.port)
        .timeout(Some(account.timeout))
        .credentials(creds);
    let transport = if ep.tls {
        // Implicit TLS, as on port 465.
        let params = tls_parameters(ep).map_err(|e| fail(Kind::Config, e))?;
        builder.tls(lettre::transport::smtp::client::Tls::Wrapper(params))
    } else if account.smtp_starttls {
        // A plaintext connection upgraded in place, as on 587.
        let params = tls_parameters(ep).map_err(|e| fail(Kind::Config, e))?;
        builder.tls(lettre::transport::smtp::client::Tls::Required(params))
    } else {
        // Bridge on loopback speaks plaintext SMTP.
        builder
    }
    .build();

    transport
        .send(draft.message)
        .await
        .with_context(|| format!("sending through SMTP at {}:{}", ep.host, ep.port))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::kind_of;
    use std::path::Path;

    fn account() -> Account {
        Account::parse(
            Some("user = \"me@example.com\""),
            Path::new("/test/config.toml"),
            &|_| None,
        )
        .unwrap()
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn split_handles_commas_spaces_and_empty_entries() {
        assert_eq!(split(&s(&["a@x, b@y"])), s(&["a@x", "b@y"]));
        assert_eq!(split(&s(&["a@x", " b@y ,, "])), s(&["a@x", "b@y"]));
        assert!(split(&s(&[",", " "])).is_empty());
        assert!(split(&[]).is_empty());
    }

    #[test]
    fn build_reports_the_message_id_and_recipients() {
        let d = build(
            &account(),
            &s(&["Ada <ada@example.com>, bob@example.com"]),
            &s(&["cc@example.com"]),
            &[],
            "Hello",
            "Body text",
        )
        .unwrap();
        assert_eq!(d.from, "me@example.com");
        assert_eq!(d.to, s(&["ada@example.com", "bob@example.com"]));
        assert_eq!(d.cc, s(&["cc@example.com"]));
        assert!(
            d.message_id.starts_with('<') && d.message_id.ends_with('>'),
            "{}",
            d.message_id
        );
        assert_eq!(
            d.recipients(),
            s(&["ada@example.com", "bob@example.com", "cc@example.com"])
        );
    }

    #[test]
    fn the_message_id_uses_the_senders_domain_and_is_unique() {
        let make = || build(&account(), &s(&["to@example.com"]), &[], &[], "s", "b").unwrap();
        let (a, b) = (make(), make());
        assert!(a.message_id.ends_with("@example.com>"), "{}", a.message_id);
        assert_ne!(a.message_id, b.message_id);
        // The id reported is the one that is on the wire.
        let wire = String::from_utf8(a.message.formatted()).unwrap();
        assert!(
            wire.contains(&format!("Message-ID: {}", a.message_id)),
            "{wire}"
        );
    }

    #[test]
    fn bcc_is_a_recipient_but_never_a_header() {
        let d = build(
            &account(),
            &s(&["to@example.com"]),
            &[],
            &s(&["secret@example.com"]),
            "Hi",
            "Body",
        )
        .unwrap();
        assert!(d.recipients().contains(&"secret@example.com".to_string()));
        assert!(
            d.message
                .envelope()
                .to()
                .iter()
                .any(|a| a.to_string() == "secret@example.com"),
            "the SMTP envelope must include the Bcc recipient"
        );
        let wire = String::from_utf8(d.message.formatted()).unwrap();
        assert!(
            !wire.contains("secret@example.com"),
            "Bcc leaked into the headers:\n{wire}"
        );
        assert!(!wire.to_lowercase().contains("bcc:"), "{wire}");
    }

    #[test]
    fn a_header_injection_attempt_in_the_subject_is_neutralized() {
        let d = build(
            &account(),
            &s(&["to@example.com"]),
            &[],
            &[],
            "Hi\r\nBcc: attacker@evil.test",
            "Body",
        )
        .unwrap();
        let wire = String::from_utf8(d.message.formatted()).unwrap();
        let headers = wire.split("\r\n\r\n").next().unwrap();
        assert!(
            !headers
                .lines()
                .any(|l| l.to_lowercase().starts_with("bcc:")),
            "{headers}"
        );
        assert!(
            d.message
                .envelope()
                .to()
                .iter()
                .all(|a| a.to_string() != "attacker@evil.test")
        );
    }

    #[test]
    fn an_invalid_address_is_a_usage_error_naming_it() {
        let err = build(&account(), &s(&["not an address"]), &[], &[], "s", "b")
            .err()
            .unwrap();
        assert_eq!(kind_of(&err), Kind::Usage);
        assert!(format!("{err:#}").contains("not an address"));
    }

    #[test]
    fn at_least_one_recipient_is_required() {
        let err = build(
            &account(),
            &s(&[" , "]),
            &[],
            &s(&["b@example.com"]),
            "s",
            "b",
        )
        .err()
        .unwrap();
        assert_eq!(kind_of(&err), Kind::Usage);
    }

    #[test]
    fn split_keeps_a_comma_inside_a_quoted_name_or_angle_brackets() {
        assert_eq!(
            split(&s(&["\"Doe, John\" <j@x.com>, b@y"])),
            s(&["\"Doe, John\" <j@x.com>", "b@y"])
        );
        assert_eq!(
            split(&s(&["Ann <a@x>,Bob <b@y>"])),
            s(&["Ann <a@x>", "Bob <b@y>"])
        );
        // An escaped quote does not end the quoted name.
        assert_eq!(
            split(&s(&["\"a\\\", b\" <c@d>"])),
            s(&["\"a\\\", b\" <c@d>"])
        );
        // An unterminated quote does not make recipients disappear.
        assert_eq!(split(&s(&["\"oops, a@x"])), s(&["\"oops, a@x"]));
    }

    #[test]
    fn a_display_name_with_a_comma_is_one_valid_recipient() {
        let d = build(
            &account(),
            &s(&["\"Doe, John\" <j@example.com>"]),
            &[],
            &[],
            "s",
            "b",
        )
        .unwrap();
        assert_eq!(d.to, s(&["j@example.com"]));
    }

    #[test]
    fn the_policy_sees_the_address_that_is_delivered_not_the_display_name() {
        // A display name that looks like an allowed address must not fool the check.
        let d = build(
            &account(),
            &s(&["\"ok@allowed.test\" <evil@evil.test>"]),
            &[],
            &[],
            "s",
            "b",
        )
        .unwrap();
        assert_eq!(d.recipients(), s(&["evil@evil.test"]));
        let policy = crate::config::Policy {
            read_only: false,
            allowed_recipients: vec!["@allowed.test".into()],
        };
        assert!(policy.check_send(&d.recipients()).is_err());
    }
}
