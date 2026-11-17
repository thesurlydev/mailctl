# mailctl

Read and send mail through Proton Mail Bridge, in Rust.

Proton does not offer a public mail API for third-party programs. Bridge is the
supported path: it runs locally, decrypts mail, and exposes plain IMAP and SMTP
on loopback. `mailctl` is a thin client of those two protocols, so it holds no
Proton-specific secrets logic and no private API.

Built to be driven by an agent: every command takes `--json`, so a caller never
parses prose. Body text is treated as untrusted data from strangers.

## Commands

| Command | What it does |
|---|---|
| `folders` | List mailboxes with message and unread counts |
| `list` | Recent messages in a mailbox, newest first, `--filter all\|unread` |
| `read <uid>` | One message in full, body decoded, attachments named |
| `search` | By subject, sender, body or since-date; terms AND together |
| `send` | Send a plain-text message through SMTP |
| `whoami` | Resolved endpoints, with the password redacted |

## Setup

Bridge must be running and logged in (it needs a paid Proton plan).

Store the Bridge password. On macOS:

```sh
security add-generic-password -a "you@example.com" -s proton-bridge -U -w
```

On Linux:

```sh
secret-tool store --label=proton-bridge service proton-bridge
```

Then point mailctl at the account. The defaults are already Bridge on loopback
(`127.0.0.1:1143` IMAP, `127.0.0.1:1025` SMTP), so most setups need one line:

```toml
# ~/.config/mailctl/config.toml
user = "you@example.com"
```

The Bridge password is **not** the account password. Bridge generates one per
install and shows it under its settings.

## Password sources

First one that resolves wins:

1. `password_command` in the config (the output of a shell command, e.g. `op read op://...`).
2. `MAILCTL_PASSWORD`.
3. The OS keychain: `security` on macOS, `secret-tool` on Linux.

The password is never written to the config file.

## Configuration

`$MAILCTL_CONFIG`, else `$XDG_CONFIG_HOME/mailctl/config.toml`, else
`~/.config/mailctl/config.toml`. Every key is optional except `user`.

```toml
user = "you@example.com"

[imap]
host = "127.0.0.1"   # Bridge
port = 1143
tls = false

[smtp]
host = "127.0.0.1"   # Bridge, or smtp.protonmail.ch:587 with starttls = true
port = 1025
tls = false
starttls = false
```

## Build

```sh
cargo build --release
cp target/release/mailctl ~/.cargo/bin/
```

`mise run gate` runs the tests and clippy together.

## Notes on Bridge

- All Mail is eventually consistent: a message moved to Trash can linger there
  for a moment.
- Moved messages can carry `\Recent`; this tool reads read/unread from `\Seen`
  only, never `\Recent`.
- SEARCH on subject and body lags FETCH by up to a minute, because Bridge
  indexes content asynchronously. Sender and date searches are immediate.
