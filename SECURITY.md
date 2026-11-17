# Security

## Reporting a problem

Please report a vulnerability privately, through this repository's GitHub
"Report a vulnerability" form (Security tab), and not in a public issue. Include
the version (`mailctl --version`) and the steps to reproduce.

## What mailctl is trying to protect

mailctl is built to be run by AI agents and scripts, so the threats it designs
against are the ones that matter for that use:

- **Prompt injection through mail.** Message bodies, subjects and sender names
  are written by strangers. mailctl returns them as data and never interprets
  them, but whatever reads that data may be tricked into acting on it. The
  strongest defense is limiting what the agent can do afterwards:
  `read_only = true` makes `send` impossible, and `allowed_recipients` limits
  where mail can go. See "Safety model" in the README.
- **Credential exposure.** The Bridge password is read from a command, an
  environment variable or the OS keychain, never from the config file, and is
  redacted from every output and from `Debug` formatting. Passing it in
  `MAILCTL_PASSWORD` exposes it to anything that can read the process
  environment, so prefer the keychain or `password_command` on a shared machine.
- **Accidental mailbox changes.** Every read command opens mailboxes with IMAP
  `EXAMINE` and fetches with `BODY.PEEK`, so reading cannot change flags, move
  or delete mail. An end-to-end test enforces this.
- **Accidental sends.** `send --dry-run` validates and previews a message
  without a password or a connection, and applies the recipient policy.

## What it does not protect against

- Anything that can run commands as you can read your keychain entry, run
  `mailctl send`, or edit `~/.config/mailctl/config.toml`. The policy is a guard
  rail for an agent that is following a harness's permissions, not a sandbox
  against a process that already has your account.
- `password_command` runs through `sh -c`. It is as trusted as the config file
  that names it; keep that file writable only by you.
- The connection to Bridge on loopback is plaintext by default, because the
  traffic never leaves the machine. Anyone with local access to the loopback
  interface can observe it. mailctl refuses to send the password in cleartext to
  any non-loopback host: such a host must use TLS.
- A timeout while sending cannot tell whether the server accepted the message.
  The error says so; check the Sent folder before retrying.
