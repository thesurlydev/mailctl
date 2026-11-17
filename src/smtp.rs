//! The write path: SMTP submission.
//!
//! Proton accepts the account's credentials on its public submission endpoint,
//! and Bridge also relays on loopback. Both are supported; the config picks.
//! The message id the server assigned comes back, since that is the only
//! proof a caller can keep that a message really left.

use anyhow::{Context as _, Result};
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as SmtpMessage, Tokio1Executor};

use crate::config::Account;

/// Split addresses on commas and trim, so `a@x, b@y` is two recipients.
fn split(addrs: &[String]) -> Vec<String> {
    addrs
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Send one plain-text message. Returns the `Message-ID` header value.
pub async fn send(
    account: &Account,
    to: &[String],
    cc: &[String],
    bcc: &[String],
    subject: &str,
    body: &str,
) -> Result<String> {
    let from: Mailbox = account
        .user
        .parse()
        .with_context(|| format!("{} is not a usable From address", account.user))?;

    // A Message-ID is generated locally (`<id@host>`) rather than left to the
    // relay, because it is what `send` reports back and what a later search
    // uses to find the message.
    let mut builder = SmtpMessage::builder()
        .from(from)
        .subject(subject)
        .message_id(None);

    // Each flag may carry several addresses. A manifest passes one string, so
    // splitting here keeps the CLI and the tool usable with the same values.
    for addr in split(to) {
        builder = builder.to(addr
            .parse()
            .with_context(|| format!("{addr} is not a valid To address"))?);
    }
    for addr in split(cc) {
        builder = builder.cc(addr
            .parse()
            .with_context(|| format!("{addr} is not a valid Cc address"))?);
    }
    for addr in split(bcc) {
        builder = builder.bcc(
            addr.parse()
                .with_context(|| format!("{addr} is not a valid Bcc address"))?,
        );
    }

    // A plain-text body is what Proton and every mail client handle most
    // predictably, and it keeps an agent from emitting accidental HTML.
    let message = builder
        .body(body.to_string())
        .context("building the message")?;
    let message_id = message
        .headers()
        .get_raw("Message-ID")
        .map(|v| v.trim().to_string())
        .unwrap_or_default();

    let ep = &account.smtp;
    let creds = Credentials::new(account.user.clone(), account.password.clone());

    let transport = if ep.tls {
        // Implicit TLS, as on Proton's 465 submission port.
        AsyncSmtpTransport::<Tokio1Executor>::relay(&ep.host)
            .context("configuring SMTP over TLS")?
            .port(ep.port)
            .credentials(creds)
            .build()
    } else if ep.starttls {
        // A plaintext connection upgraded in place, as on 587. `lettre` only
        // reaches this through `relay`'s TLS parameters, so build it by hand.
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&ep.host)
            .context("configuring SMTP over STARTTLS")?
            .port(ep.port)
            .credentials(creds)
            .build()
    } else {
        // Bridge on loopback speaks plaintext SMTP.
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&ep.host)
            .port(ep.port)
            .credentials(creds)
            .build()
    };

    transport
        .send(message)
        .await
        .with_context(|| format!("sending through SMTP at {}:{}", ep.host, ep.port))?;

    Ok(message_id)
}
