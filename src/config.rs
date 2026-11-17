//! Where the account and its password come from.
//!
//! Two rules shape this: the password never lives in a config file, and the
//! config works on both the Mac and the Linux machine. So settings come from
//! a small TOML file (or environment variables), and the password comes from
//! the OS keychain, an environment variable, or a named command, in that
//! order of preference for each machine.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

/// One endpoint (IMAP or SMTP).
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// `true` wraps the connection in TLS (implicit TLS, SMTP 465).
    pub tls: bool,
    /// `true` upgrades a plaintext connection with STARTTLS (SMTP 587). Only
    /// the send path uses it; IMAP against Bridge is plain on loopback.
    pub starttls: bool,
}

/// The account `mailctl` acts as.
#[derive(Clone, Debug)]
pub struct Account {
    pub user: String,
    pub password: String,
    pub imap: Endpoint,
    pub smtp: Endpoint,
}

/// The on-disk shape. Every field is optional so a machine only states what
/// differs from the defaults.
#[derive(Debug, Default, Deserialize)]
struct File {
    user: Option<String>,
    password_command: Option<String>,
    #[serde(default)]
    imap: FileEndpoint,
    #[serde(default)]
    smtp: FileEndpoint,
}

#[derive(Debug, Default, Deserialize)]
struct FileEndpoint {
    host: Option<String>,
    port: Option<u16>,
    tls: Option<bool>,
    starttls: Option<bool>,
}

impl Account {
    /// The config path: `$MAILCTL_CONFIG`, else
    /// `$XDG_CONFIG_HOME/mailctl/config.toml`, else `~/.config/mailctl/config.toml`.
    pub fn config_path() -> PathBuf {
        if let Ok(p) = std::env::var("MAILCTL_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home().join(".config"));
        base.join("mailctl").join("config.toml")
    }

    pub fn load() -> Result<Account> {
        let path = Self::config_path();
        let file: File = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
            }
            // No config is fine: the defaults are Bridge on loopback, and the
            // user and password can come from the environment.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        let user = std::env::var("MAILCTL_USER")
            .ok()
            .or(file.user)
            .context("no account set: set `user` in the config file or MAILCTL_USER")?;

        let password = resolve_password(&file.password_command, &path, &user)?;

        // Bridge on loopback is the default for both directions, because it is
        // the only endpoint that needs no extra setup once Bridge is running.
        let imap = Endpoint {
            host: file.imap.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: file.imap.port.unwrap_or(1143),
            tls: file.imap.tls.unwrap_or(false),
            starttls: file.imap.starttls.unwrap_or(false),
        };
        let smtp = Endpoint {
            host: file.smtp.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: file.smtp.port.unwrap_or(1025),
            tls: file.smtp.tls.unwrap_or(false),
            starttls: file.smtp.starttls.unwrap_or(false),
        };

        Ok(Account {
            user,
            password,
            imap,
            smtp,
        })
    }
}

/// The password, from the first source that has it.
///
/// Order matters: an explicit `password_command` in the config is the most
/// deliberate choice, so it wins. Then the environment, which is how a
/// scheduled run supplies it. Then the OS keychain, which is where a person
/// puts it once and forgets about it.
fn resolve_password(
    password_command: &Option<String>,
    config_path: &std::path::Path,
    user: &str,
) -> Result<String> {
    if let Some(cmd) = password_command {
        return run_command(cmd)
            .with_context(|| format!("running password_command from {}", config_path.display()));
    }

    if let Ok(p) = std::env::var("MAILCTL_PASSWORD")
        && !p.is_empty()
    {
        return Ok(p);
    }

    if let Some(p) = keychain_password(user) {
        return Ok(p);
    }

    bail!(
        "no password found for {user}: set MAILCTL_PASSWORD, store it in the keychain \
         (`security add-generic-password -a {user} -s proton-bridge -U -w` on macOS, \
         `secret-tool store --label=proton-bridge service proton-bridge` on Linux), \
         or set `password_command` in {}",
        config_path.display()
    )
}

/// `security find-generic-password` on macOS, `secret-tool lookup` on Linux.
/// `None` when the item is absent, so the caller can report what to do.
fn keychain_password(user: &str) -> Option<String> {
    let service =
        std::env::var("MAILCTL_KEYCHAIN_SERVICE").unwrap_or_else(|_| "proton-bridge".into());

    if cfg!(target_os = "macos") {
        return run_quiet(
            "security",
            &[
                "find-generic-password".into(),
                "-a".into(),
                user.into(),
                "-s".into(),
                service,
                "-w".into(),
            ],
        );
    }

    if cfg!(target_os = "linux") {
        let found = run_quiet("secret-tool", &["lookup".into(), "service".into(), service])?;
        return (!found.is_empty()).then_some(found);
    }

    None
}

fn run_quiet(program: &str, args: &[String]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    let _ = Duration::from_secs(5);
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// A command whose stdout is the password. Supports `op read op://...` and
/// any other secret manager, plus plain shell for a `pass`-style store.
fn run_command(cmd: &str) -> Result<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .with_context(|| format!("spawning `{cmd}`"))?;
    if !out.status.success() {
        bail!(
            "`{cmd}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        bail!("`{cmd}` produced no output");
    }
    Ok(s)
}

fn home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}
