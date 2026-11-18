# mailctl

Read and send Proton Mail from the command line, built to be driven by AI agents.

Proton has no public mail API for third-party programs. [Proton Mail Bridge](https://proton.me/mail/bridge)
is the supported path: it runs on your machine, decrypts your mail, and exposes plain IMAP and SMTP on
loopback. `mailctl` is a small client of those two protocols. It holds no Proton-specific secrets logic and
uses no private API.

What makes it suitable for an agent or a script:

- **One JSON value on stdout.** With `--json`, stdout is always exactly one JSON value, failures included
  (`{"error": "...", "kind": "..."}`), and the exit code names the kind of failure.
- **Reading is provably read-only.** Mailboxes are opened with IMAP `EXAMINE` and bodies fetched with
  `BODY.PEEK`, so no read command can mark, move or delete a message.
- **Sending is guarded.** `send --dry-run` previews a message without a password or a connection, and a
  config policy (`read_only`, `allowed_recipients`) can make sending impossible or limit where it goes.
- **Mail is treated as hostile input.** Bodies are returned as data, cut at a character limit, and
  documented as untrusted for the agent reading them.

Agents: read [AGENTS.md](AGENTS.md).

## Install

Requires Rust 1.98 or later and [Proton Mail Bridge](https://proton.me/mail/bridge) (a paid Proton plan),
running and logged in.

```sh
cargo install --git https://github.com/thesurlydev/mailctl
```

Or from a checkout: `cargo build --release`, then copy `target/release/mailctl` onto your `PATH`
(`mise run install` copies it to `~/.cargo/bin`).

## Setup

1. Find the Bridge password. It is **not** your Proton account password: Bridge generates one per
   install and shows it in its account settings.
2. Store it in the OS keychain. On macOS:

   ```sh
   security add-generic-password -a "you@example.com" -s proton-bridge -U -w
   ```

   On Linux (needs `secret-tool`, from libsecret):

   ```sh
   secret-tool store --label=proton-bridge service proton-bridge
   ```

3. Point mailctl at the account. The defaults are Bridge on loopback (`127.0.0.1:1143` IMAP,
   `127.0.0.1:1025` SMTP), so most setups need one line:

   ```toml
   # ~/.config/mailctl/config.toml
   user = "you@example.com"
   ```

4. Check it: `mailctl whoami` shows the resolved settings without touching the network, and
   `mailctl folders` confirms Bridge and the password work.

## Commands

| Command | What it does |
|---|---|
| `folders` | List mailboxes with message and unread counts |
| `list` | Recent messages in a mailbox, newest first. `--mailbox`, `--limit`, `--filter all\|unread` |
| `read <uid>` | One message in full: decoded body, attachments named. `--mailbox`, `--body-limit <chars>`, `--links` (also the `http(s)` links of the HTML part, which the text body drops) |
| `search` | By `--subject`, `--from`, `--body` or `--since YYYY-MM-DD`; terms AND together; empty terms are ignored |
| `send` | Send a plain-text message: `--to`, `--cc`, `--bcc`, `--subject`, `--body` (`-` reads stdin), `--dry-run` |
| `whoami` | Resolved settings, with the password redacted |

Global flags: `--json` and `--timeout <secs>`.

```sh
mailctl list --limit 5
mailctl list --filter unread --json
mailctl read 4211 --json
mailctl search --from billing@example.com --since 2026-11-01
mailctl send --dry-run --to ada@example.com --subject "Lunch" --body "Noon?"
printf 'a long body\n- with a list\n' | mailctl send --to ada@example.com --subject "Notes" --body -
```

UIDs are per mailbox and are what `list` and `search` print and `read` takes.

### JSON output

`list` and `search` print an array of `{uid, from, subject, date, seen, flagged}`. `folders` prints an
array of `{name, messages, unseen}`. `read` prints:

```json
{
  "uid": 4211,
  "mailbox": "INBOX",
  "from": "Ada Lovelace <ada@example.com>",
  "to": "you@example.com",
  "cc": "",
  "subject": "Lunch",
  "date": "2026-11-17 09:30",
  "message_id": "<abc123@example.com>",
  "body": "Noon?",
  "attachments": ["menu.pdf (application/pdf)"],
  "truncated": false
}
```

`read --links` adds `"links": [{"text": "...", "url": "https://..."}]`, the `http` and `https` anchors of the
HTML part in document order (at most 200, a repeated URL once). The text body drops links, so a mail such as a job
alert names its items there but only `--links` says where they point. Without the flag the field is absent.
Link text and URLs are written by the sender: untrusted, and mailctl never fetches them.

`date` is the time the server received the message. `body` is text written by the sender: untrusted.
`send` prints `{"sent": true, "message_id": "<...>"}`. `send --dry-run` prints `{"sent": false,
"dry_run": true, ...}` with the message that would be sent.

These field names are a contract; renaming one is a breaking change.

### Errors and exit codes

With `--json`, a failure prints `{"error": "...", "kind": "..."}` on stdout. Without it, `error: ...` goes
to stderr. Either way the exit code is:

| Code | `kind` | Meaning |
|---|---|---|
| 0 | | Success |
| 1 | `other` | Anything unexpected |
| 2 | `usage` | Bad arguments, or a malformed address or date |
| 3 | `config` | No account, no password, or an invalid config file |
| 4 | `unreachable` | Nothing is listening at the IMAP or SMTP endpoint: Bridge is not running |
| 5 | `auth` | Bridge rejected the login. Check you used the Bridge password |
| 6 | `not_found` | No such mailbox or message UID |
| 7 | `refused` | The policy (`read_only`, `allowed_recipients`) refused the send |
| 8 | `timeout` | The command did not finish within the timeout (default 30 seconds). For `send`, once the server was contacted the message may or may not have gone out: check Sent before retrying |

A reader that closes the pipe early (`mailctl list | head`) exits 0 quietly.

## Configuration

`$MAILCTL_CONFIG`, else `$XDG_CONFIG_HOME/mailctl/config.toml`, else `~/.config/mailctl/config.toml`.
Every key is optional except `user` (which `MAILCTL_USER` can supply). Unknown keys are an error, so a
typo cannot silently turn a safety setting off.

```toml
user = "you@example.com"
# password_command = "op read op://Private/Bridge/password"
# read_only = true                              # refuse every send
# allowed_recipients = ["me@example.com", "@my-company.com"]
# timeout_secs = 30

[imap]
host = "127.0.0.1"   # Bridge
port = 1143
tls = false
# ca_cert = "~/bridge-cert.pem"   # extra CA to trust, for Bridge's self-signed certificate

[smtp]
host = "127.0.0.1"   # Bridge
port = 1025
tls = false          # implicit TLS (port 465 style)
starttls = false     # upgrade with STARTTLS (port 587 style); not both
# ca_cert = "~/bridge-cert.pem"
```

Environment variables: `MAILCTL_CONFIG`, `MAILCTL_USER`, `MAILCTL_PASSWORD`, `MAILCTL_TIMEOUT`,
`MAILCTL_READONLY` (any value except `0`, `false`, `no` or `off` switches read-only on, so a typo fails closed;
it can never switch it off, so a stray variable cannot undo the config file), `MAILCTL_KEYCHAIN_SERVICE` (default `proton-bridge`).

### Password sources

First one that resolves wins:

1. `password_command` in the config: the output of a shell command, for example `op read op://...` or
   `pass show bridge`.
2. `MAILCTL_PASSWORD`.
3. The OS keychain: `security` on macOS, `secret-tool` on Linux.

The password is never written to the config file, is only looked up for commands that connect (not for
`whoami` or `send --dry-run`), and is redacted from every output.

## Safety model

mailctl is meant to be given to software that reads strangers' mail, so it defends against that software
being talked into doing damage.

- **Reads cannot change the mailbox.** `EXAMINE` plus `BODY.PEEK`. An end-to-end test fails if any read
  command issues `SELECT`, `STORE`, `COPY`, `MOVE`, `DELETE`, `EXPUNGE` or a non-peek fetch.
- **Sending is separable.** For an agent that only needs to read, set `read_only = true`: `send` then fails
  with `refused` and never opens a connection. For one that may send, set `allowed_recipients`; every To,
  Cc and Bcc address must match an entry (a full address, or `@domain` for anyone at exactly that domain).
- **Preview before sending.** `send --dry-run` validates the addresses, applies the policy, and prints the
  message, with no password and no network.
- **Mail cannot drive your terminal.** Without `--json`, every field written by a sender is printed with its
  control characters (escape sequences, carriage returns, bidi overrides) shown as the replacement character
  U+FFFD, so a message cannot rewrite the screen, set the window title or reorder what you read. The
  `send --dry-run` preview and error messages are cleaned the same way, so nothing in a message you are about
  to approve can hide. JSON output is unchanged: it carries the text exactly, escaped by the encoder.
- **Untrusted text stays data.** A message body can contain instructions aimed at whatever reads it. See
  [AGENTS.md](AGENTS.md) for the rules an agent should follow, and [SECURITY.md](SECURITY.md) for the threat
  model and what is out of scope.
- **Bounded.** Bodies are cut at 20000 characters (`--body-limit`), and every command has a 30 second
  timeout, so a stalled Bridge cannot hang the caller. The process exits when the timeout fires, even if stdin
  (`--body -`) is never closed.
- **Header safety.** Bcc recipients are delivered but never appear in the headers, and a line break in a
  subject cannot inject a header. The Message-ID uses the sender's domain, not this machine's hostname.

### TLS

Bridge on loopback is reached without TLS by default, since the traffic never leaves the machine. mailctl will
not send the password in cleartext anywhere else: a non-loopback `host` with TLS off is a config error. If you run
Bridge in SSL mode, set `tls = true` for that endpoint and point `ca_cert` at Bridge's certificate (export it
from Bridge's settings). Trust is the bundled Mozilla roots plus that one file; there is no switch to turn
verification off. TLS is on rustls, with no OpenSSL dependency. This path is covered by unit tests of the
connector but has not been exercised against a live Bridge in SSL mode.

## Using it with an agent

[AGENTS.md](AGENTS.md) is written for the agent: how to call mailctl, the hard rules for untrusted mail and
for sending, and what each error means. Point your harness at it, or paste it into the agent's instructions.

If your harness defines tools as command templates, wrap `list`, `read`, `search`, `send --dry-run` and `send`
as separate tools, with `--json` in each, and require approval for the one that sends. mailctl ignores an empty
search term, so a template can pass every `search` flag and leave the unused ones as empty strings.

Any harness that can run shell commands can use mailctl as is. In Claude Code, for example, allow the read
commands and leave `send` out, so it prompts:

```json
{
  "permissions": {
    "allow": [
      "Bash(mailctl folders:*)",
      "Bash(mailctl list:*)",
      "Bash(mailctl read:*)",
      "Bash(mailctl search:*)",
      "Bash(mailctl whoami:*)"
    ]
  }
}
```

Put `--json` after the subcommand (`mailctl list --json`) so prefix rules like these match.

## Notes on Bridge

- All Mail is eventually consistent: a message moved to Trash can linger there for a moment.
- Moved messages can carry `\Recent`; this tool reads read/unread from `\Seen` only, never `\Recent`.
- SEARCH on subject and body lags FETCH by up to a minute, because Bridge indexes content asynchronously.
  Sender and date searches are immediate. mailctl never retries an empty search.
- Attachments are listed by name and type; they are not downloaded.
- Non-ASCII search terms are sent as UTF-8 inside a quoted IMAP string. Whether Bridge matches them has not been
  verified; search by sender or date if a non-ASCII subject or body search finds nothing.

## Development

```sh
mise run gate     # cargo fmt --check, clippy with warnings denied, unit and end-to-end tests
```

The end-to-end tests run the real binary against small fake IMAP and SMTP servers, so the gate needs no live
mailbox. Checking a real Bridge is manual. Contributor and agent notes are in [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
