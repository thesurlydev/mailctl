//! Where the account, its password and the safety policy come from.
//!
//! Settings come from a small TOML file and a few environment variables. The
//! password never lives in the config file: it comes from a named command, the
//! environment, or the OS keychain. It is resolved lazily, so commands that do
//! not authenticate (`whoami`, `send --dry-run`) never touch the keychain.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use tokio::process::Command;

use crate::error::{Kind, fail};

pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// A secret that cannot be printed by accident: `Debug` redacts it.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Where environment variables are read from. Tests pass a fake one, because
/// changing the real environment is unsafe in a multi-threaded process.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// A variable that is set and not empty.
fn var(env: Env, key: &str) -> Option<String> {
    env(key).filter(|v| !v.is_empty())
}

/// Whether a safety switch given in the environment is on. Anything that is
/// not clearly "off" counts as on, so a typo such as `MAILCTL_READONLY=y`
/// fails closed instead of silently leaving sending enabled.
fn switched_on(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Loopback hosts are the only ones that may use a plaintext connection.
fn is_loopback(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// One endpoint (IMAP or SMTP).
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// `true` wraps the connection in TLS (implicit TLS, SMTP 465, IMAP 993).
    pub tls: bool,
    /// An extra CA certificate (PEM) to trust, for Bridge's self-signed one.
    pub ca_cert: Option<PathBuf>,
}

/// What the account is allowed to do. It exists so an agent that reads
/// untrusted mail cannot be talked into sending it somewhere.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// `send` is refused outright.
    pub read_only: bool,
    /// When not empty, every recipient must match: either a full address, or
    /// `@example.com` for anyone at that domain.
    pub allowed_recipients: Vec<String>,
}

impl Policy {
    /// Fails when sending is switched off altogether.
    pub fn check_enabled(&self) -> Result<()> {
        if self.read_only {
            return Err(fail(
                Kind::Refused,
                anyhow!("sending is disabled: read_only is set (config file or MAILCTL_READONLY)"),
            ));
        }
        Ok(())
    }

    pub fn check_send(&self, recipients: &[String]) -> Result<()> {
        self.check_enabled()?;
        if self.allowed_recipients.is_empty() {
            return Ok(());
        }
        for r in recipients {
            if !self.allows(r) {
                return Err(fail(
                    Kind::Refused,
                    anyhow!("{r} is not in allowed_recipients"),
                ));
            }
        }
        Ok(())
    }

    fn allows(&self, addr: &str) -> bool {
        let addr = addr.trim().to_lowercase();
        self.allowed_recipients.iter().any(|entry| {
            let entry = entry.trim().to_lowercase();
            match entry.strip_prefix('@') {
                Some(domain) => addr.rsplit_once('@').is_some_and(|(_, d)| d == domain),
                None => addr == entry,
            }
        })
    }
}

/// The account `mailctl` acts as. It holds no password; ask for it with
/// [`Account::password`] at the moment it is needed.
#[derive(Clone, Debug)]
pub struct Account {
    pub user: String,
    pub imap: Endpoint,
    pub smtp: Endpoint,
    /// Upgrade a plaintext SMTP connection with STARTTLS (port 587).
    pub smtp_starttls: bool,
    pub policy: Policy,
    /// How long a whole command, and each helper process, may take.
    pub timeout: Duration,
    pub config_path: PathBuf,
    password_command: Option<String>,
    env_password: Option<Secret>,
    keychain_service: String,
}

/// The on-disk shape. Every field is optional so a machine only states what
/// differs from the defaults. Unknown keys are an error, so a typo such as
/// `read-only` cannot silently turn a safety setting off.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    user: Option<String>,
    password_command: Option<String>,
    read_only: Option<bool>,
    allowed_recipients: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    imap: FileImap,
    #[serde(default)]
    smtp: FileSmtp,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileImap {
    host: Option<String>,
    port: Option<u16>,
    tls: Option<bool>,
    ca_cert: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSmtp {
    host: Option<String>,
    port: Option<u16>,
    tls: Option<bool>,
    starttls: Option<bool>,
    ca_cert: Option<String>,
}

/// The config path: `$MAILCTL_CONFIG`, else `$XDG_CONFIG_HOME/mailctl/config.toml`,
/// else `~/.config/mailctl/config.toml`.
pub fn config_path(env: Env) -> PathBuf {
    if let Some(p) = var(env, "MAILCTL_CONFIG") {
        return PathBuf::from(p);
    }
    let base = var(env, "XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home(env).join(".config"));
    base.join("mailctl").join("config.toml")
}

fn home(env: Env) -> PathBuf {
    var(env, "HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `~/x` becomes `$HOME/x`; anything else is used as written.
fn expand_home(path: &str, env: Env) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home(env).join(rest),
        None => PathBuf::from(path),
    }
}

impl Account {
    /// Load from the real environment and the config file.
    pub fn load() -> Result<Account> {
        let path = config_path(&process_env);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            // No config is fine: the defaults are Bridge on loopback, and the
            // user can come from the environment.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(fail(
                    Kind::Config,
                    anyhow::Error::new(e).context(format!("reading {}", path.display())),
                ));
            }
        };
        Self::parse(text.as_deref(), &path, &process_env)
    }

    /// Build the account from config text (if any) and an environment.
    pub fn parse(text: Option<&str>, path: &Path, env: Env) -> Result<Account> {
        Self::parse_inner(text, path, env).map_err(|e| fail(Kind::Config, e))
    }

    fn parse_inner(text: Option<&str>, path: &Path, env: Env) -> Result<Account> {
        let file: File = match text {
            Some(text) => {
                toml::from_str(text).with_context(|| format!("parsing {}", path.display()))?
            }
            None => File::default(),
        };

        let user = var(env, "MAILCTL_USER")
            .or(file.user)
            .context("no account set: set `user` in the config file or MAILCTL_USER")?;

        // Bridge on loopback is the default for both directions, because it is
        // the only endpoint that needs no extra setup once Bridge is running.
        let imap = Endpoint {
            host: file.imap.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: file.imap.port.unwrap_or(1143),
            tls: file.imap.tls.unwrap_or(false),
            ca_cert: file.imap.ca_cert.map(|p| expand_home(&p, env)),
        };
        let smtp = Endpoint {
            host: file.smtp.host.unwrap_or_else(|| "127.0.0.1".into()),
            port: file.smtp.port.unwrap_or(1025),
            tls: file.smtp.tls.unwrap_or(false),
            ca_cert: file.smtp.ca_cert.map(|p| expand_home(&p, env)),
        };
        let smtp_starttls = file.smtp.starttls.unwrap_or(false);
        if smtp.tls && smtp_starttls {
            bail!("smtp.tls and smtp.starttls are alternatives: set only one");
        }
        // The password goes over this connection, so a plaintext one is only
        // acceptable where the traffic never leaves the machine.
        if !imap.tls && !is_loopback(&imap.host) {
            bail!(
                "imap.host {} is not a loopback address, so the connection must use TLS: set \
                 imap.tls = true (and imap.ca_cert for a self-signed certificate). mailctl will \
                 not send the password in cleartext over a network",
                imap.host
            );
        }
        if !smtp.tls && !smtp_starttls && !is_loopback(&smtp.host) {
            bail!(
                "smtp.host {} is not a loopback address, so the connection must use TLS: set \
                 smtp.tls = true or smtp.starttls = true (and smtp.ca_cert for a self-signed \
                 certificate). mailctl will not send the password in cleartext over a network",
                smtp.host
            );
        }

        // The environment can switch read-only mode on but never off, so a
        // setting in the file cannot be undone by a stray variable. Read this
        // one directly rather than through `var`: an empty value is not the
        // same as an unset one for a safety switch, and must fail closed too.
        let read_only = file.read_only.unwrap_or(false)
            || env("MAILCTL_READONLY").is_some_and(|v| switched_on(&v));
        let policy = Policy {
            read_only,
            allowed_recipients: file.allowed_recipients.unwrap_or_default(),
        };

        let timeout_secs = match var(env, "MAILCTL_TIMEOUT") {
            Some(v) => v
                .trim()
                .parse::<u64>()
                .with_context(|| format!("MAILCTL_TIMEOUT={v} is not a number of seconds"))?,
            None => file.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        };
        if timeout_secs == 0 {
            bail!("the timeout must be at least 1 second");
        }

        Ok(Account {
            user,
            imap,
            smtp,
            smtp_starttls,
            policy,
            timeout: Duration::from_secs(timeout_secs),
            config_path: path.to_path_buf(),
            password_command: file.password_command,
            env_password: var(env, "MAILCTL_PASSWORD").map(Secret),
            keychain_service: var(env, "MAILCTL_KEYCHAIN_SERVICE")
                .unwrap_or_else(|| "proton-bridge".into()),
        })
    }

    /// The password, from the first source that has it.
    ///
    /// Order matters: an explicit `password_command` in the config is the most
    /// deliberate choice, so it wins. Then the environment, which is how a
    /// scheduled run supplies it. Then the OS keychain, which is where a person
    /// puts it once and forgets about it.
    pub async fn password(&self) -> Result<Secret> {
        if let Some(cmd) = &self.password_command {
            return run_command(cmd, self.timeout)
                .await
                .with_context(|| format!("password_command in {}", self.config_path.display()));
        }

        if let Some(p) = &self.env_password {
            return Ok(p.clone());
        }

        if let Some(p) = keychain_password(&self.user, &self.keychain_service, self.timeout).await {
            return Ok(Secret(p));
        }

        Err(fail(
            Kind::Config,
            anyhow!(
                "no password found for {user}: set MAILCTL_PASSWORD, store it in the keychain \
                 (`security add-generic-password -a {user} -s {service} -U -w` on macOS, \
                 `secret-tool store --label={service} service {service}` on Linux), \
                 or set `password_command` in {path}",
                user = self.user,
                service = self.keychain_service,
                path = self.config_path.display()
            ),
        ))
    }
}

/// `security find-generic-password` on macOS, `secret-tool lookup` on Linux.
/// `None` when the item is absent, so the caller can report what to do.
async fn keychain_password(user: &str, service: &str, timeout: Duration) -> Option<String> {
    if cfg!(target_os = "macos") {
        return run_quiet(
            "security",
            &["find-generic-password", "-a", user, "-s", service, "-w"],
            timeout,
        )
        .await;
    }

    if cfg!(target_os = "linux") {
        return run_quiet("secret-tool", &["lookup", "service", service], timeout).await;
    }

    None
}

/// Run a program and return its trimmed stdout, or `None` on any failure.
async fn run_quiet(program: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Terminate a timed-out command and everything it started.
fn terminate_command(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // The child is its own group leader (see `process_group(0)` above),
        // so a negative pid signals the whole group. SIGKILL (9 on both macOS
        // and Linux) rather than SIGTERM: a helper that has timed out has
        // nothing to clean up, and a child may have inherited an ignored
        // SIGTERM from a shell that trapped it.
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        unsafe {
            kill(-(pid as i32), 9);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// A command whose stdout is the password: `op read op://...`, `pass show ...`,
/// or any other secret manager. It runs through `sh -c`, so it is as trusted as
/// the config file that names it.
async fn run_command(cmd: &str, timeout: Duration) -> Result<Secret> {
    // The command text is not repeated in any error: a config that embeds a
    // literal secret in it would otherwise print that secret.
    let config_error = |e: anyhow::Error| fail(Kind::Config, e);
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Put the shell, and anything it forks, into a group of its own so a
    // timeout can terminate all of it. Killing only `sh` leaves a grandchild
    // holding the stdout pipe open.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    let child = command
        .spawn()
        .map_err(|e| config_error(anyhow::Error::new(e).context("spawning the command")))?;
    let pid = child.id();
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            terminate_command(pid);
            fail(
                Kind::Timeout,
                anyhow!("the command did not finish in {}s", timeout.as_secs()),
            )
        })?
        .map_err(|e| config_error(anyhow::Error::new(e).context("waiting for the command")))?;
    if !out.status.success() {
        return Err(config_error(anyhow!(
            "the command failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        return Err(config_error(anyhow!("the command produced no output")));
    }
    Ok(Secret(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::kind_of;
    use std::collections::HashMap;

    /// An environment backed by a map, so no test touches the real one.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn parse(text: &str, env: &[(&str, &str)]) -> Result<Account> {
        Account::parse(Some(text), Path::new("/test/config.toml"), &env_of(env))
    }

    #[test]
    fn defaults_are_bridge_on_loopback() {
        let a = parse("user = \"me@example.com\"", &[]).unwrap();
        assert_eq!(a.user, "me@example.com");
        assert_eq!(
            (a.imap.host.as_str(), a.imap.port, a.imap.tls),
            ("127.0.0.1", 1143, false)
        );
        assert_eq!(
            (a.smtp.host.as_str(), a.smtp.port, a.smtp.tls),
            ("127.0.0.1", 1025, false)
        );
        assert!(!a.smtp_starttls);
        assert!(!a.policy.read_only);
        assert!(a.policy.allowed_recipients.is_empty());
        assert_eq!(a.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn the_user_can_come_from_the_environment_alone() {
        let a = Account::parse(
            None,
            Path::new("/x"),
            &env_of(&[("MAILCTL_USER", "env@example.com")]),
        )
        .unwrap();
        assert_eq!(a.user, "env@example.com");
    }

    #[test]
    fn the_environment_user_overrides_the_file() {
        let a = parse(
            "user = \"file@example.com\"",
            &[("MAILCTL_USER", "env@example.com")],
        )
        .unwrap();
        assert_eq!(a.user, "env@example.com");
    }

    #[test]
    fn a_missing_user_is_a_config_error() {
        let err = Account::parse(None, Path::new("/x"), &env_of(&[])).unwrap_err();
        assert_eq!(kind_of(&err), Kind::Config);
        assert!(format!("{err:#}").contains("no account set"));
    }

    #[test]
    fn an_unknown_key_is_rejected_instead_of_ignored() {
        let err = parse("user = \"a@b\"\nread-only = true", &[]).unwrap_err();
        assert_eq!(kind_of(&err), Kind::Config);
        assert!(format!("{err:#}").contains("read-only"));
    }

    #[test]
    fn imap_has_no_starttls_option() {
        let err = parse("user = \"a@b\"\n[imap]\nstarttls = true", &[]).unwrap_err();
        assert!(format!("{err:#}").contains("starttls"));
    }

    #[test]
    fn smtp_tls_and_starttls_are_exclusive() {
        let err = parse("user = \"a@b\"\n[smtp]\ntls = true\nstarttls = true", &[]).unwrap_err();
        assert!(format!("{err:#}").contains("alternatives"));
    }

    #[test]
    fn the_environment_can_turn_read_only_on_but_not_off() {
        let on = parse("user = \"a@b\"", &[("MAILCTL_READONLY", "1")]).unwrap();
        assert!(on.policy.read_only);
        let stays = parse(
            "user = \"a@b\"\nread_only = true",
            &[("MAILCTL_READONLY", "0")],
        )
        .unwrap();
        assert!(stays.policy.read_only);
        let off = parse("user = \"a@b\"", &[("MAILCTL_READONLY", "no")]).unwrap();
        assert!(!off.policy.read_only);
    }

    #[test]
    fn the_timeout_comes_from_the_environment_then_the_file() {
        let file = parse("user = \"a@b\"\ntimeout_secs = 7", &[]).unwrap();
        assert_eq!(file.timeout, Duration::from_secs(7));
        let env = parse(
            "user = \"a@b\"\ntimeout_secs = 7",
            &[("MAILCTL_TIMEOUT", "12")],
        )
        .unwrap();
        assert_eq!(env.timeout, Duration::from_secs(12));
        assert!(parse("user = \"a@b\"\ntimeout_secs = 0", &[]).is_err());
        assert!(parse("user = \"a@b\"", &[("MAILCTL_TIMEOUT", "soon")]).is_err());
    }

    #[test]
    fn ca_cert_expands_a_leading_tilde() {
        let a = parse(
            "user = \"a@b\"\n[imap]\nca_cert = \"~/bridge.pem\"",
            &[("HOME", "/home/test")],
        )
        .unwrap();
        assert_eq!(a.imap.ca_cert, Some(PathBuf::from("/home/test/bridge.pem")));
    }

    #[test]
    fn config_path_prefers_mailctl_config_then_xdg_then_home() {
        let p = |pairs: &[(&str, &str)]| config_path(&env_of(pairs));
        assert_eq!(
            p(&[("MAILCTL_CONFIG", "/etc/m.toml")]),
            PathBuf::from("/etc/m.toml")
        );
        assert_eq!(
            p(&[("XDG_CONFIG_HOME", "/xdg"), ("HOME", "/h")]),
            PathBuf::from("/xdg/mailctl/config.toml")
        );
        assert_eq!(
            p(&[("HOME", "/h")]),
            PathBuf::from("/h/.config/mailctl/config.toml")
        );
    }

    #[tokio::test]
    async fn password_command_wins_over_the_environment() {
        let a = parse(
            "user = \"a@b\"\npassword_command = \"printf from-command\"",
            &[("MAILCTL_PASSWORD", "from-env")],
        )
        .unwrap();
        assert_eq!(a.password().await.unwrap().expose(), "from-command");
    }

    #[tokio::test]
    async fn the_environment_password_is_used_when_there_is_no_command() {
        let a = parse("user = \"a@b\"", &[("MAILCTL_PASSWORD", "from-env")]).unwrap();
        assert_eq!(a.password().await.unwrap().expose(), "from-env");
    }

    #[tokio::test]
    async fn an_empty_environment_password_counts_as_unset() {
        let a = parse(
            "user = \"nobody@example.invalid\"",
            &[
                ("MAILCTL_PASSWORD", ""),
                (
                    "MAILCTL_KEYCHAIN_SERVICE",
                    "mailctl-test-service-that-does-not-exist",
                ),
            ],
        )
        .unwrap();
        let err = a.password().await.unwrap_err();
        assert_eq!(kind_of(&err), Kind::Config);
        assert!(format!("{err:#}").contains("no password found"));
    }

    #[tokio::test]
    async fn a_failing_password_command_reports_its_stderr() {
        let a = parse(
            "user = \"a@b\"\npassword_command = \"echo nope >&2; exit 3\"",
            &[],
        )
        .unwrap();
        let err = a.password().await.unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("nope"), "{text}");
        assert!(text.contains("password_command"), "{text}");
    }

    #[tokio::test]
    async fn a_hanging_password_command_times_out() {
        let mut a = parse("user = \"a@b\"\npassword_command = \"exec sleep 30\"", &[]).unwrap();
        a.timeout = Duration::from_millis(200);
        let err = a.password().await.unwrap_err();
        assert_eq!(kind_of(&err), Kind::Timeout);
    }

    #[test]
    fn the_password_never_appears_in_debug_output() {
        assert_eq!(
            format!("{:?}", Secret("hunter2".into())),
            "Secret(<redacted>)"
        );
        let a = parse("user = \"a@b\"", &[("MAILCTL_PASSWORD", "hunter2")]).unwrap();
        assert!(!format!("{a:?}").contains("hunter2"));
    }

    #[test]
    fn policy_read_only_refuses_every_send() {
        let p = Policy {
            read_only: true,
            allowed_recipients: vec![],
        };
        let err = p.check_send(&["a@example.com".into()]).unwrap_err();
        assert_eq!(kind_of(&err), Kind::Refused);
    }

    #[test]
    fn policy_allowlist_matches_addresses_and_domains_case_insensitively() {
        let p = Policy {
            read_only: false,
            allowed_recipients: vec!["Me@Example.com".into(), "@corp.test".into()],
        };
        assert!(
            p.check_send(&["me@example.com".into(), "anyone@CORP.test".into()])
                .is_ok()
        );
        let err = p
            .check_send(&["me@example.com".into(), "x@evil.test".into()])
            .unwrap_err();
        assert_eq!(kind_of(&err), Kind::Refused);
        assert!(format!("{err:#}").contains("x@evil.test"));
        // A domain entry must not match a look-alike suffix or a subdomain.
        assert!(p.check_send(&["a@notcorp.test".into()]).is_err());
        assert!(p.check_send(&["a@sub.corp.test".into()]).is_err());
    }

    #[test]
    fn an_empty_allowlist_allows_everyone() {
        assert!(
            Policy::default()
                .check_send(&["x@anywhere.test".into()])
                .is_ok()
        );
    }

    #[test]
    fn an_unrecognized_read_only_value_fails_closed() {
        // A typo in a safety switch must not leave sending enabled.
        for value in [
            "1", "true", "yes", "on", "y", "enabled", "TRUE", "banana", "", "  ",
        ] {
            let a = parse("user = \"a@b\"", &[("MAILCTL_READONLY", value)]).unwrap();
            assert!(a.policy.read_only, "{value} should switch read-only on");
        }
        for value in ["0", "false", "no", "off", "OFF", " 0 ", "  false  "] {
            let a = parse("user = \"a@b\"", &[("MAILCTL_READONLY", value)]).unwrap();
            assert!(!a.policy.read_only, "{value} should leave it off");
        }
    }

    #[test]
    fn a_plaintext_connection_to_a_remote_host_is_refused() {
        let imap = parse("user = \"a@b\"\n[imap]\nhost = \"mail.example.com\"", &[]).unwrap_err();
        assert!(format!("{imap:#}").contains("must use TLS"), "{imap:#}");
        assert_eq!(kind_of(&imap), Kind::Config);
        let smtp = parse("user = \"a@b\"\n[smtp]\nhost = \"10.0.0.5\"", &[]).unwrap_err();
        assert!(format!("{smtp:#}").contains("smtp.tls = true or smtp.starttls = true"));
        // TLS or STARTTLS makes a remote host fine, and loopback never needs it.
        assert!(
            parse(
                "user = \"a@b\"\n[imap]\nhost = \"mail.example.com\"\ntls = true",
                &[]
            )
            .is_ok()
        );
        assert!(
            parse(
                "user = \"a@b\"\n[smtp]\nhost = \"mail.example.com\"\nstarttls = true",
                &[]
            )
            .is_ok()
        );
        for host in [
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
        ] {
            let text =
                format!("user = \"a@b\"\n[imap]\nhost = \"{host}\"\n[smtp]\nhost = \"{host}\"");
            assert!(parse(&text, &[]).is_ok(), "{host}");
        }
        // Look-alikes are not loopback.
        assert!(
            parse(
                "user = \"a@b\"\n[imap]\nhost = \"localhost.evil.test\"",
                &[]
            )
            .is_err()
        );
        assert!(
            parse(
                "user = \"a@b\"\n[imap]\nhost = \"127.0.0.1.evil.test\"",
                &[]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_failing_password_command_is_a_config_error() {
        let a = parse(
            "user = \"a@b\"\npassword_command = \"echo nope >&2; exit 3\"",
            &[],
        )
        .unwrap();
        assert_eq!(kind_of(&a.password().await.unwrap_err()), Kind::Config);
    }

    #[tokio::test]
    async fn a_failing_password_command_never_echoes_its_own_text() {
        // A config that embeds a literal secret in the command must not have it
        // printed back in an error.
        let a = parse(
            "user = \"a@b\"\npassword_command = \"echo hunter2-literal >/dev/null; exit 1\"",
            &[],
        )
        .unwrap();
        let err = a.password().await.unwrap_err();
        assert!(!format!("{err:#}").contains("hunter2-literal"), "{err:#}");
        assert_eq!(kind_of(&err), Kind::Config);

        let silent = parse("user = \"a@b\"\npassword_command = \"true\"", &[]).unwrap();
        let err = silent.password().await.unwrap_err();
        assert_eq!(kind_of(&err), Kind::Config);
        assert!(format!("{err:#}").contains("no output"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_password_command_kills_the_whole_group() {
        // A shell that forks a child. Killing only `sh` (which `kill_on_drop`
        // already does) would leave the child alive and holding the pipe;
        // the group kill must take it too. The shell writes the CHILD's pid
        // so the test checks the grandchild, not the shell.
        let dir = std::env::temp_dir().join(format!(
            "mailctl-group-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("child.pid");
        let text = format!(
            "user = \"a@b\"\ntimeout_secs = 1\n\
             password_command = \"sleep 9 & echo $! > {}; wait\"\n",
            pidfile.display()
        );
        let a = parse(&text, &[]).unwrap();
        let err = a.password().await.unwrap_err();
        assert_eq!(kind_of(&err), Kind::Timeout);

        let child = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pidfile) {
                    let pid = text.trim();
                    let alive = std::process::Command::new("ps")
                        .args(["-o", "pid=", "-p", pid])
                        .output()
                        .map(|o| !o.stdout.is_empty())
                        .unwrap_or(false);
                    if !alive {
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            child.is_ok(),
            "the forked child survived its shell's timeout"
        );
    }
}
