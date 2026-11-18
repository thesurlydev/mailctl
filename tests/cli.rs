//! End-to-end tests for the real `mailctl` binary.
//!
//! Nothing here talks to Proton or the keychain. The two fake servers speak
//! just enough IMAP and SMTP for the CLI, and every child process gets an empty
//! environment plus the handful of variables a test sets explicitly.

use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

const USER: &str = "me@example.com";
const PASSWORD: &str = "bridge-secret";
const NO_KEYCHAIN: (&str, &str) = (
    "MAILCTL_KEYCHAIN_SERVICE",
    "mailctl-test-service-that-does-not-exist",
);

fn password_env() -> [(&'static str, &'static str); 1] {
    [("MAILCTL_PASSWORD", PASSWORD)]
}

fn json_error(out: &Output, code: i32, kind: &str) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "stderr should be empty with --json: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON value: {e}: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert!(v.is_object(), "expected a JSON object: {v}");
    assert_eq!(v["kind"], kind, "wrong kind: {v}");
    assert!(
        v["error"].as_str().is_some_and(|s| !s.is_empty()),
        "missing error: {v}"
    );
    v
}

fn text_error(out: &Output, code: i32) -> String {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.starts_with("error: "),
        "not an error line: {stderr:?}"
    );
    stderr
}

fn json_ok(out: &Output) -> Value {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not one JSON value: {e}: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn text_ok(out: &Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run the binary with a cleared environment and only `env` added.
fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    run_with_input(args, env, None)
}

fn run_with_input(args: &[&str], env: &[(&str, &str)], input: Option<&[u8]>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailctl"));
    cmd.env_clear();
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn mailctl");
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(input).unwrap();
    }
    child.wait_with_output().expect("wait for mailctl")
}

fn run_f(f: &Fixture, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let config = f.config.to_string_lossy();
    let mut env: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 1);
    env.push(("MAILCTL_CONFIG", config.as_ref()));
    env.extend_from_slice(extra);
    run(args, &env)
}

fn run_f_stdin(f: &Fixture, args: &[&str], extra: &[(&str, &str)], input: &[u8]) -> Output {
    let config = f.config.to_string_lossy();
    let mut env: Vec<(&str, &str)> = Vec::with_capacity(extra.len() + 1);
    env.push(("MAILCTL_CONFIG", config.as_ref()));
    env.extend_from_slice(extra);
    run_with_input(args, &env, Some(input))
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    dir: PathBuf,
    config: PathBuf,
    imap_port: u16,
    smtp_port: u16,
    imap: Option<ImapServer>,
    smtp: Option<SmtpServer>,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mailctl-cli-{}-{}-{}",
            std::process::id(),
            nonce,
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self {
            config: dir.join("config.toml"),
            dir,
            imap_port: 1143,
            smtp_port: 1025,
            imap: None,
            smtp: None,
        }
    }

    fn write_raw_config(&self, text: &str) {
        fs::write(&self.config, text).unwrap();
    }

    /// `root` holds top-level keys, `smtp_extra` keys inside `[smtp]`.
    fn write_config(&self, root: &str, smtp_extra: &str) {
        self.write_raw_config(&format!(
            "user = \"{USER}\"\n{root}\n\
             [imap]\nhost = \"127.0.0.1\"\nport = {}\n\
             [smtp]\nhost = \"127.0.0.1\"\nport = {}\n{smtp_extra}\n",
            self.imap_port, self.smtp_port
        ));
    }

    fn start_imap(&mut self) {
        let server = ImapServer::start();
        self.imap_port = server.port();
        self.imap = Some(server);
    }

    fn start_hung_imap(&mut self) {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let listener = Listener::start(move |_stream, stop| {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(20));
            }
        });
        self.imap_port = listener.port();
        self.imap = Some(ImapServer { listener, commands });
    }

    fn start_smtp(&mut self) {
        let server = SmtpServer::start();
        self.smtp_port = server.port();
        self.smtp = Some(server);
    }

    fn imap_commands(&self) -> Vec<String> {
        self.imap.as_ref().unwrap().commands.lock().unwrap().clone()
    }

    fn smtp_recipients(&self) -> Vec<String> {
        self.smtp
            .as_ref()
            .unwrap()
            .recipients
            .lock()
            .unwrap()
            .clone()
    }

    fn smtp_data(&self) -> String {
        self.smtp.as_ref().unwrap().data.lock().unwrap().clone()
    }

    fn smtp_connections(&self) -> usize {
        self.smtp
            .as_ref()
            .unwrap()
            .connections
            .load(Ordering::Relaxed)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.imap.take();
        self.smtp.take();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Accept loop and handler-join bookkeeping shared by both fake servers.
struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    handlers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Listener {
    fn start<F>(handler: F) -> Self
    where
        F: Fn(TcpStream, Arc<AtomicBool>) + Send + Sync + 'static,
    {
        let handler = Arc::new(handler);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let handlers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let accept_stop = stop.clone();
        let accept_handlers = handlers.clone();
        let accept = thread::spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let mut list = accept_handlers.lock().unwrap();
                        list.retain(|h| !h.is_finished());
                        let handler = handler.clone();
                        let stop = accept_stop.clone();
                        list.push(thread::spawn(move || handler(stream, stop)));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            stop,
            accept: Some(accept),
            handlers,
        }
    }

    fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        let handlers = std::mem::take(&mut *self.handlers.lock().unwrap());
        for handler in handlers {
            let _ = handler.join();
        }
    }
}

fn set_timeouts(stream: &TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
}

/// A line with a per-read timeout, so handlers exit promptly when stopped.
/// `partial` survives a timeout: clients may send a tag and the rest of a
/// command in separate writes, and a partial line must not be discarded.
fn read_line(reader: &mut BufReader<TcpStream>, partial: &mut Vec<u8>) -> Option<String> {
    if let Some(end) = partial.iter().position(|&b| b == b'\n') {
        let bytes: Vec<u8> = partial.drain(..=end).collect();
        return Some(
            String::from_utf8_lossy(&bytes)
                .trim_end_matches(['\r', '\n'])
                .to_string(),
        );
    }
    let mut buf = [0u8; 4096];
    match reader.read(&mut buf) {
        Ok(0) => None,
        Ok(n) => {
            partial.extend_from_slice(&buf[..n]);
            read_line(reader, partial)
        }
        Err(_) => None,
    }
}

fn reply(writer: &mut TcpStream, text: &str) {
    let _ = writer.write_all(text.as_bytes());
    let _ = writer.flush();
}

struct ImapServer {
    listener: Listener,
    commands: Arc<Mutex<Vec<String>>>,
}

impl ImapServer {
    fn start() -> Self {
        let commands = Arc::new(Mutex::new(Vec::new()));
        let listener = {
            let commands = commands.clone();
            Listener::start(move |stream, stop| serve_imap(stream, stop, &commands))
        };
        Self { listener, commands }
    }

    fn port(&self) -> u16 {
        self.listener.port()
    }
}

struct Seed {
    uid: u32,
    raw: String,
    flags: &'static str,
}

fn seed(uid: u32, from: &str, subject: &str, body: &str, flags: &'static str) -> Seed {
    // A body that starts with a tag is sent as HTML.
    let kind = if body.starts_with('<') {
        "html"
    } else {
        "plain"
    };
    Seed {
        uid,
        flags,
        raw: format!(
            "From: {from}\r\nTo: Me <me@example.com>\r\nSubject: {subject}\r\n\
             Date: Tue, 17 Nov 2026 09:30:00 +0000\r\nMessage-ID: <m{uid}@example.com>\r\n\
             MIME-Version: 1.0\r\nContent-Type: text/{kind}; charset=utf-8\r\n\
             Content-Transfer-Encoding: 8bit\r\n\r\n{body}"
        ),
    }
}

fn mailbox() -> Vec<Seed> {
    vec![
        seed(
            10,
            "Ada <ada@example.com>",
            "Plain note",
            "Hello there.",
            "\\Seen",
        ),
        seed(
            20,
            "=?UTF-8?Q?=C3=89mile_Dupont?= <emile@example.com>",
            "=?UTF-8?B?Q2Fmw6kgbWVudQ==?=",
            "Bonjour.",
            "",
        ),
        seed(
            30,
            "Long <long@example.com>",
            "Long body",
            &"\u{20ac}".repeat(30_000),
            "\\Seen \\Flagged",
        ),
        seed(
            40,
            "Flag <flag@example.com>",
            "Flagged",
            "<p>Unread and <a href=\"https://jobs.example.com/view/7?trk=a&amp;ref=b\">flagged</a>.</p>",
            "\\Flagged",
        ),
        // Hostile terminal input, reachable only by UID so list and search
        // results stay as they are: an encoded title-setting escape in the
        // sender name, an encoded line break in the subject, and raw escapes,
        // a carriage return and a bidi override in the body.
        seed(
            50,
            "=?UTF-8?Q?Bank=1B]0;pwned=07?= <bank@example.com>",
            "=?UTF-8?Q?Invoice=1B[2J=0D=0Aspoofed?=",
            "Pay \u{1b}[31mnow\u{1b}[0m\rOVERWRITE\nsecond\tline \u{202e}txt.exe",
            "",
        ),
        seed(
            60,
            "Links <links@example.com>",
            "Links",
            "<p><a href=\"https://x.example/\">click&#27;[2J here</a></p>",
            "",
        ),
    ]
}

fn header_of(raw: &str) -> &str {
    let end = raw.find("\r\n\r\n").unwrap() + 4;
    &raw[..end]
}

fn first_arg(command: &str) -> &str {
    let rest = command.split_once(' ').map(|(_, rest)| rest).unwrap_or("");
    let rest = rest.trim_start();
    if let Some(rest) = rest.strip_prefix('"') {
        rest.split('"').next().unwrap_or("")
    } else {
        rest.split_whitespace().next().unwrap_or("")
    }
}

fn quoted(name: &str) -> String {
    if name.contains('/') || name.contains(' ') {
        format!("\"{name}\"")
    } else {
        name.to_string()
    }
}

fn mailbox_count(name: &str) -> (u32, u32) {
    match name {
        "Sent" => (3, 0),
        "Labels/Work" => (2, 1),
        _ => (4, 2),
    }
}

fn search_result(criteria: &str) -> Vec<u32> {
    let c = criteria.to_ascii_uppercase();
    if c.contains("UNSEEN") {
        vec![20, 40]
    } else if c.contains("SUBJECT") || c.contains("FROM") {
        vec![20]
    } else if c.contains("BODY") {
        vec![30]
    } else {
        vec![10, 20, 30, 40]
    }
}

fn serve_imap(stream: TcpStream, stop: Arc<AtomicBool>, commands: &Mutex<Vec<String>>) {
    set_timeouts(&stream);
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut partial = Vec::new();
    reply(&mut writer, "* OK IMAP4rev1 ready\r\n");
    let messages = mailbox();

    while !stop.load(Ordering::Relaxed) {
        let Some(line) = read_line(&mut reader, &mut partial) else {
            continue;
        };
        let (tag, command) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        commands.lock().unwrap().push(command.to_string());
        let upper = command.to_ascii_uppercase();

        if upper.starts_with("CAPABILITY") {
            reply(
                &mut writer,
                &format!("* CAPABILITY IMAP4rev1 UIDPLUS\r\n{tag} OK CAPABILITY done\r\n"),
            );
        } else if upper.starts_with("LOGIN ") {
            if command.contains(PASSWORD) {
                reply(&mut writer, &format!("{tag} OK LOGIN done\r\n"));
            } else {
                reply(
                    &mut writer,
                    &format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n"),
                );
            }
        } else if upper.starts_with("LIST ") {
            for name in ["INBOX", "Sent", "Labels/Work"] {
                reply(
                    &mut writer,
                    &format!("* LIST (\\HasNoChildren) \"/\" \"{name}\"\r\n"),
                );
            }
            reply(&mut writer, &format!("{tag} OK LIST done\r\n"));
        } else if upper.starts_with("STATUS ") {
            let name = first_arg(command);
            let (messages, unseen) = mailbox_count(name);
            reply(
                &mut writer,
                &format!(
                    "* STATUS {} (MESSAGES {messages} UNSEEN {unseen})\r\n{tag} OK STATUS done\r\n",
                    quoted(name)
                ),
            );
        } else if upper.starts_with("EXAMINE ") || upper.starts_with("SELECT ") {
            let name = first_arg(command);
            if ["INBOX", "Sent", "Labels/Work"].contains(&name) {
                let (exists, _) = mailbox_count(name);
                reply(
                    &mut writer,
                    &format!(
                        "* {exists} EXISTS\r\n* FLAGS (\\Seen \\Flagged)\r\n{tag} OK [READ-ONLY] EXAMINE done\r\n"
                    ),
                );
            } else {
                reply(
                    &mut writer,
                    &format!("{tag} NO [NONEXISTENT] no such mailbox\r\n"),
                );
            }
        } else if upper.starts_with("UID SEARCH ") {
            let criteria = command.strip_prefix("UID SEARCH ").unwrap_or("");
            let found = search_result(criteria)
                .into_iter()
                .fold(String::new(), |mut s, uid| {
                    if !s.is_empty() {
                        s.push(' ');
                    }
                    s.push_str(&uid.to_string());
                    s
                });
            reply(
                &mut writer,
                &format!("* SEARCH {found}\r\n{tag} OK SEARCH done\r\n"),
            );
        } else if upper.starts_with("UID FETCH ") {
            let mut parts = command.split_whitespace();
            let _ = parts.next(); // UID
            let _ = parts.next(); // FETCH
            let set = parts.next().unwrap_or("");
            let query = parts.collect::<Vec<_>>().join(" ");
            let full = query.contains("BODY.PEEK[]");
            for token in set.split(',') {
                let uid: u32 = token.parse().unwrap_or(0);
                let Some(msg) = messages.iter().find(|m| m.uid == uid) else {
                    continue;
                };
                // The section name in a FETCH response is BODY, even when the
                // client asked for BODY.PEEK; PEEK is a request modifier only.
                let section = if full {
                    "BODY[]"
                } else {
                    "BODY[HEADER.FIELDS (FROM SUBJECT DATE)]"
                };
                let data = if full {
                    msg.raw.as_bytes()
                } else {
                    header_of(&msg.raw).as_bytes()
                };
                reply(
                    &mut writer,
                    &format!(
                        "* {} FETCH (UID {uid} FLAGS ({}) INTERNALDATE \"17-Nov-2026 09:30:00 +0000\" {section} {{{}}}\r\n",
                        msg.uid,
                        msg.flags,
                        data.len()
                    ),
                );
                let _ = writer.write_all(data);
                reply(&mut writer, ")\r\n");
            }
            reply(&mut writer, &format!("{tag} OK FETCH done\r\n"));
        } else if upper.starts_with("LOGOUT") {
            reply(
                &mut writer,
                &format!("* BYE logging out\r\n{tag} OK LOGOUT done\r\n"),
            );
            break;
        } else {
            reply(&mut writer, &format!("{tag} BAD unsupported command\r\n"));
        }
    }
}

struct SmtpServer {
    listener: Listener,
    recipients: Arc<Mutex<Vec<String>>>,
    data: Arc<Mutex<String>>,
    connections: Arc<AtomicUsize>,
}

impl SmtpServer {
    fn start() -> Self {
        let recipients = Arc::new(Mutex::new(Vec::new()));
        let data = Arc::new(Mutex::new(String::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let listener = {
            let recipients = recipients.clone();
            let data = data.clone();
            let connections = connections.clone();
            Listener::start(move |stream, stop| {
                connections.fetch_add(1, Ordering::Relaxed);
                serve_smtp(stream, stop, &recipients, &data)
            })
        };
        Self {
            listener,
            recipients,
            data,
            connections,
        }
    }

    fn port(&self) -> u16 {
        self.listener.port()
    }
}

fn serve_smtp(
    stream: TcpStream,
    stop: Arc<AtomicBool>,
    recipients: &Mutex<Vec<String>>,
    data: &Mutex<String>,
) {
    set_timeouts(&stream);
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut partial = Vec::new();
    reply(&mut writer, "220 ready\r\n");

    while !stop.load(Ordering::Relaxed) {
        let Some(line) = read_line(&mut reader, &mut partial) else {
            continue;
        };
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("EHLO") {
            reply(
                &mut writer,
                "250-localhost\r\n250-AUTH PLAIN LOGIN\r\n250 OK\r\n",
            );
        } else if upper.starts_with("HELO") {
            reply(&mut writer, "250 localhost\r\n");
        } else if upper.starts_with("AUTH PLAIN") {
            // PLAIN accepts either an initial response or a follow-up line.
            if !line[10..].trim().is_empty() || read_line(&mut reader, &mut partial).is_some() {
                reply(&mut writer, "235 ok\r\n");
            }
        } else if upper.starts_with("AUTH LOGIN") {
            reply(&mut writer, "334 VXNlcm5hbWU6\r\n");
            let _ = read_line(&mut reader, &mut partial);
            reply(&mut writer, "334 UGFzc3dvcmQ6\r\n");
            let _ = read_line(&mut reader, &mut partial);
            reply(&mut writer, "235 ok\r\n");
        } else if upper.starts_with("MAIL FROM") {
            reply(&mut writer, "250 ok\r\n");
        } else if upper.starts_with("RCPT TO") {
            let addr = line
                .split_once('<')
                .and_then(|(_, rest)| rest.split_once('>'))
                .map(|(addr, _)| addr.to_ascii_lowercase())
                .unwrap_or_else(|| line.to_ascii_lowercase());
            recipients.lock().unwrap().push(addr);
            reply(&mut writer, "250 ok\r\n");
        } else if upper.starts_with("DATA") {
            reply(&mut writer, "354 end with .\r\n");
            let mut message = String::new();
            loop {
                let Some(chunk) = read_line(&mut reader, &mut partial) else {
                    continue;
                };
                if chunk == "." {
                    break;
                }
                let chunk = chunk.strip_prefix("..").unwrap_or(&chunk);
                message.push_str(chunk);
                message.push_str("\r\n");
            }
            *data.lock().unwrap() = message;
            reply(&mut writer, "250 ok\r\n");
        } else if upper.starts_with("QUIT") {
            reply(&mut writer, "221 bye\r\n");
            break;
        } else if upper.starts_with("NOOP") || upper.starts_with("RSET") {
            reply(&mut writer, "250 ok\r\n");
        } else {
            reply(&mut writer, "500 unknown command\r\n");
        }
    }
}

#[test]
fn whoami_json_without_a_server_or_password_redacts_the_password() {
    let f = Fixture::new();
    f.write_config("", "");
    let out = run_f(&f, &["whoami", "--json"], &[NO_KEYCHAIN]);
    let v = json_ok(&out);
    assert_eq!(v["read_only"], false);
    assert_eq!(v["allowed_recipients"], serde_json::json!([]));
    assert_eq!(v["timeout_secs"], 30);
    assert_eq!(v["password"], "<redacted>");

    let out = run_f(
        &f,
        &["whoami", "--json"],
        &[("MAILCTL_PASSWORD", "secret-password")],
    );
    let v = json_ok(&out);
    let text = v.to_string();
    assert!(!text.contains("secret-password"), "{text}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("secret-password"), "{stderr}");
}

#[test]
fn folders_json_and_text_show_names_and_counts() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    let out = run_f(&f, &["folders", "--json"], &password_env());
    assert_eq!(
        json_ok(&out),
        serde_json::json!([
            {"name": "INBOX", "messages": 4, "unseen": 2},
            {"name": "Labels/Work", "messages": 2, "unseen": 1},
            {"name": "Sent", "messages": 3, "unseen": 0},
        ])
    );

    let out = run_f(&f, &["folders"], &password_env());
    let text = text_ok(&out);
    assert!(text.contains("INBOX"), "{text}");
    assert!(text.contains("2 unread"), "{text}");
    assert!(text.contains("Labels/Work"), "{text}");
}

#[test]
fn list_json_is_newest_first_with_seen_and_flagged() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    let out = run_f(&f, &["list", "--json"], &password_env());
    let v = json_ok(&out);
    let rows = v.as_array().unwrap();
    let uids: Vec<u32> = rows
        .iter()
        .map(|r| r["uid"].as_u64().unwrap() as u32)
        .collect();
    assert_eq!(uids, vec![40, 30, 20, 10]);
    let by_uid = |uid| rows.iter().find(|r| r["uid"] == uid).unwrap();
    assert_eq!(by_uid(40)["seen"], false);
    assert_eq!(by_uid(40)["flagged"], true);
    assert_eq!(by_uid(30)["seen"], true);
    assert_eq!(by_uid(30)["flagged"], true);
    assert_eq!(by_uid(20)["seen"], false);
    assert_eq!(by_uid(10)["seen"], true);

    let out = run_f(
        &f,
        &["list", "--filter", "unread", "--json"],
        &password_env(),
    );
    let v = json_ok(&out);
    let uids: Vec<u32> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uid"].as_u64().unwrap() as u32)
        .collect();
    assert_eq!(uids, vec![40, 20]);
}

#[test]
fn read_decodes_rfc2047_subject_and_display_name() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    let out = run_f(&f, &["read", "20", "--json"], &password_env());
    let v = json_ok(&out);
    assert_eq!(v["subject"], "Café menu");
    assert_eq!(v["from"], "Émile Dupont <emile@example.com>");
    assert_eq!(v["uid"], 20);
    assert!(v["body"].as_str().unwrap().contains("Bonjour"));
    assert_eq!(v["truncated"], false);
}

#[test]
fn read_links_returns_the_anchors_of_an_html_message_only_when_asked() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");

    let out = run_f(&f, &["read", "40", "--json", "--links"], &password_env());
    let v = json_ok(&out);
    assert_eq!(
        v["links"],
        serde_json::json!([
            {"text": "flagged", "url": "https://jobs.example.com/view/7?trk=a&ref=b"}
        ])
    );
    // The text body still drops the href, which is why the flag exists.
    assert!(v["body"].as_str().unwrap().contains("flagged"));
    assert!(!v["body"].as_str().unwrap().contains("jobs.example.com"));

    // Without the flag the field is absent, so the default JSON is unchanged.
    let out = run_f(&f, &["read", "40", "--json"], &password_env());
    assert!(json_ok(&out).get("links").is_none());

    // A plain-text message has no links, and says so.
    let out = run_f(&f, &["read", "10", "--json", "--links"], &password_env());
    assert_eq!(json_ok(&out)["links"], serde_json::json!([]));

    let out = run_f(&f, &["read", "40", "--links"], &password_env());
    let text = text_ok(&out);
    assert!(
        text.contains("Links:\n  flagged  https://jobs.example.com/view/7?trk=a&ref=b"),
        "{text}"
    );
}

/// Characters that can move the cursor, rewrite the screen or reorder text
/// in a terminal: every control character but a newline or tab, plus the
/// Unicode bidi controls.
fn terminal_controls(text: &str) -> Vec<char> {
    text.chars()
        .filter(|&c| {
            (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

#[test]
fn text_output_strips_terminal_controls_that_json_keeps() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");

    let out = run_f(&f, &["read", "50"], &password_env());
    let text = text_ok(&out);
    assert_eq!(terminal_controls(&text), Vec::<char>::new(), "{text:?}");
    // Each control shows as U+FFFD, so nothing is hidden, and a line break in
    // a header cannot start a line of its own.
    assert!(
        text.contains("From:    Bank\u{fffd}]0;pwned\u{fffd} <bank@example.com>\n"),
        "{text:?}"
    );
    assert!(
        text.contains("Subject: Invoice\u{fffd}[2J spoofed\n"),
        "{text:?}"
    );
    assert!(
        text.contains(
            "Pay \u{fffd}[31mnow\u{fffd}[0m\u{fffd}OVERWRITE\nsecond\tline \u{fffd}txt.exe"
        ),
        "{text:?}"
    );

    let out = run_f(&f, &["read", "60", "--links"], &password_env());
    let text = text_ok(&out);
    assert_eq!(terminal_controls(&text), Vec::<char>::new(), "{text:?}");
    assert!(
        text.contains("  click\u{fffd}[2J here  https://x.example/"),
        "{text:?}"
    );

    // The person approving a send must see everything it would send.
    let out = run_f(
        &f,
        &[
            "send",
            "--dry-run",
            "--to",
            "ada@example.com",
            "--subject",
            "Re: \u{1b}[8mhidden",
            "--body",
            "ok\u{1b}[8m then hidden\r\n",
        ],
        &[],
    );
    let text = text_ok(&out);
    assert_eq!(terminal_controls(&text), Vec::<char>::new(), "{text:?}");
    assert!(
        text.contains("Subject: Re: \u{fffd}[8mhidden\n"),
        "{text:?}"
    );
    assert!(text.contains("ok\u{fffd}[8m then hidden\n"), "{text:?}");

    // JSON is data for a program, escaped by the encoder, so it keeps the
    // message exactly as sent.
    let out = run_f(&f, &["read", "50", "--json"], &password_env());
    let v = json_ok(&out);
    assert_eq!(v["subject"], "Invoice\u{1b}[2J\r\nspoofed");
    assert!(v["body"].as_str().unwrap().contains("\u{1b}[31mnow"));
}

#[test]
fn read_cuts_a_long_non_ascii_body_by_characters() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");

    let out = run_f(&f, &["read", "30", "--json"], &password_env());
    let v = json_ok(&out);
    assert_eq!(v["truncated"], true);
    assert_eq!(v["body"].as_str().unwrap().chars().count(), 20_000);

    let out = run_f(
        &f,
        &["read", "30", "--json", "--body-limit", "0"],
        &password_env(),
    );
    let v = json_ok(&out);
    assert_eq!(v["truncated"], false);
    assert_eq!(v["body"].as_str().unwrap().chars().count(), 30_000);

    let out = run_f(
        &f,
        &["read", "30", "--json", "--body-limit", "5"],
        &password_env(),
    );
    let v = json_ok(&out);
    assert_eq!(v["body"].as_str().unwrap(), "\u{20ac}".repeat(5));
    assert_eq!(v["truncated"], true);
}

#[test]
fn search_with_subject_from_and_since_returns_rows() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    for args in [
        vec!["search", "--subject", "Café", "--json"],
        vec!["search", "--from", "emile@example.com", "--json"],
        vec!["search", "--since", "2026-11-01", "--json"],
    ] {
        let out = run_f(&f, &args, &password_env());
        let v = json_ok(&out);
        assert!(!v.as_array().unwrap().is_empty(), "{args:?}: {v}");
    }
    let commands = f.imap_commands();
    assert!(commands.iter().any(|c| c.contains("UID SEARCH SUBJECT")));
    assert!(commands.iter().any(|c| c.contains("UID SEARCH FROM")));
    assert!(
        commands
            .iter()
            .any(|c| c.contains("UID SEARCH SINCE 01-Nov-2026"))
    );
}

#[test]
fn an_empty_or_bad_date_search_is_usage_and_sends_no_search() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    for args in [
        vec!["search", "--json"],
        vec!["search", "--since", "17-11-2026", "--json"],
    ] {
        let out = run_f(&f, &args, &password_env());
        json_error(&out, 2, "usage");
    }
    assert!(
        f.imap_commands()
            .iter()
            .all(|c| !c.to_ascii_uppercase().starts_with("UID SEARCH")),
        "{:?}",
        f.imap_commands()
    );
}

#[test]
fn reading_never_changes_mailbox_or_message_state() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    for args in [
        vec!["list", "--json"],
        vec!["read", "20", "--json"],
        vec!["search", "--subject", "note", "--json"],
        vec!["folders", "--json"],
    ] {
        json_ok(&run_f(&f, &args, &password_env()));
    }
    let commands = f.imap_commands();
    assert!(
        commands.iter().any(|c| c.starts_with("EXAMINE")),
        "{commands:?}"
    );
    for c in &commands {
        // The verb is the first word, or the second after a `UID` prefix.
        let upper = c.to_ascii_uppercase();
        let mut words = upper.split_whitespace();
        let first = words.next().unwrap_or("");
        let verb = if first == "UID" {
            words.next().unwrap_or("")
        } else {
            first
        };
        assert!(
            [
                "CAPABILITY",
                "LOGIN",
                "LOGOUT",
                "LIST",
                "STATUS",
                "EXAMINE",
                "SEARCH",
                "FETCH"
            ]
            .contains(&verb),
            "unexpected or mutating command: {c}"
        );
        if verb == "FETCH" {
            // Every fetch must say how it reads. A body fetch must peek; an
            // RFC822 or other implicit-read form is refused outright.
            assert!(
                upper.contains("BODY.PEEK["),
                "a fetch that does not peek: {c}"
            );
            assert!(
                !upper.contains("RFC822") && !upper.replace("BODY.PEEK[", "").contains("BODY["),
                "a fetch that marks the message as seen: {c}"
            );
        }
    }
}

#[test]
fn wrong_password_json_is_auth_with_empty_stderr() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    let out = run_f(
        &f,
        &["folders", "--json"],
        &[("MAILCTL_PASSWORD", "wrong-password")],
    );
    json_error(&out, 5, "auth");
}

#[test]
fn unreachable_bridge_json_is_unreachable() {
    let mut f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    f.imap_port = listener.local_addr().unwrap().port();
    drop(listener);
    f.write_config("", "");
    let out = run_f(&f, &["folders", "--json"], &password_env());
    json_error(&out, 4, "unreachable");
}

#[test]
fn missing_message_and_mailbox_are_not_found() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    let out = run_f(&f, &["read", "999", "--json"], &password_env());
    json_error(&out, 6, "not_found");
    let out = run_f(
        &f,
        &["list", "--mailbox", "Nope", "--json"],
        &password_env(),
    );
    json_error(&out, 6, "not_found");
}

#[test]
fn missing_user_password_and_unknown_key_are_config_errors() {
    let f = Fixture::new();
    f.write_raw_config("[imap]\nport = 1143\n");
    let out = run_f(&f, &["whoami", "--json"], &[NO_KEYCHAIN]);
    json_error(&out, 3, "config");

    f.write_config("", "");
    let out = run_f(&f, &["folders", "--json"], &[NO_KEYCHAIN]);
    json_error(&out, 3, "config");

    f.write_raw_config("user = \"me@example.com\"\nread-only = true\n");
    let out = run_f(&f, &["whoami", "--json"], &[NO_KEYCHAIN]);
    let v = json_error(&out, 3, "config");
    assert!(v["error"].as_str().unwrap().contains("read-only"), "{v}");
}

#[test]
fn errors_without_json_go_to_stderr_as_error_lines() {
    let f = Fixture::new();
    f.write_raw_config("user = \"me@example.com\"\nread-only = true\n");
    let out = run_f(&f, &["whoami"], &[NO_KEYCHAIN]);
    let text = text_error(&out, 3);
    assert!(text.contains("read-only"), "{text}");
}

#[test]
fn clap_usage_error_with_json_is_a_json_error_object() {
    let out = run(
        &["send", "--to", "a@example.com", "--body", "hello", "--json"],
        &[NO_KEYCHAIN],
    );
    let v = json_error(&out, 2, "usage");
    assert!(v["error"].as_str().unwrap().contains("--subject"), "{v}");
}

#[test]
fn help_and_version_exit_zero_with_text() {
    for args in [vec!["--help"], vec!["--version"]] {
        let out = run(&args, &[NO_KEYCHAIN]);
        let text = text_ok(&out);
        assert!(!text.trim().is_empty(), "{args:?}");
    }
}

#[test]
fn a_silent_imap_server_times_out_after_one_second() {
    let mut f = Fixture::new();
    f.start_hung_imap();
    f.write_config("", "");

    let start = Instant::now();
    let out = run_f(
        &f,
        &["folders", "--json", "--timeout", "1"],
        &password_env(),
    );
    let elapsed = start.elapsed();
    json_error(&out, 8, "timeout");
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed <= Duration::from_secs(4),
        "{elapsed:?}"
    );
}

#[test]
fn mailctl_timeout_environment_variable_is_honored() {
    let mut f = Fixture::new();
    f.start_hung_imap();
    f.write_config("", "");
    let start = Instant::now();
    let out = run_f(
        &f,
        &["folders", "--json"],
        &[("MAILCTL_PASSWORD", PASSWORD), ("MAILCTL_TIMEOUT", "1")],
    );
    let elapsed = start.elapsed();
    json_error(&out, 8, "timeout");
    assert!(
        elapsed >= Duration::from_secs(1) && elapsed <= Duration::from_secs(4),
        "{elapsed:?}"
    );
}

#[test]
fn send_submits_to_all_recipients_without_a_bcc_header() {
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--to",
            "b@example.com",
            "--cc",
            "c@example.com",
            "--bcc",
            "secret@example.com",
            "--subject",
            "Hi",
            "--body",
            "hello",
            "--json",
        ],
        &password_env(),
    );
    let v = json_ok(&out);
    assert_eq!(v["sent"], true);
    let id = v["message_id"].as_str().unwrap();
    assert!(id.starts_with('<') && id.ends_with('>'), "{id}");

    let recipients = f.smtp_recipients();
    for addr in [
        "a@example.com",
        "b@example.com",
        "c@example.com",
        "secret@example.com",
    ] {
        assert!(
            recipients.iter().any(|r| r == addr),
            "{addr} missing from {recipients:?}"
        );
    }
    let data = f.smtp_data();
    assert!(data.contains(id), "message id missing from DATA: {data}");
    assert!(!data.contains("secret@example.com"), "Bcc leaked: {data}");
    assert!(
        !data.to_ascii_lowercase().contains("bcc:"),
        "Bcc header: {data}"
    );
    assert_eq!(f.smtp_connections(), 1);
}

#[test]
fn send_body_dash_reads_stdin() {
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("", "");
    let out = run_f_stdin(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--subject",
            "Hi",
            "--body",
            "-",
            "--json",
        ],
        &password_env(),
        b"body from stdin\r\n",
    );
    json_ok(&out);
    assert!(
        f.smtp_data().contains("body from stdin"),
        "{}",
        f.smtp_data()
    );
}

#[test]
fn a_subject_injection_cannot_add_a_bcc_header_or_recipient() {
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--subject",
            "Hi\r\nBcc: x@evil.test",
            "--body",
            "hello",
            "--json",
        ],
        &password_env(),
    );
    json_ok(&out);
    let data = f.smtp_data();
    let headers = data.split("\r\n\r\n").next().unwrap();
    assert!(
        !headers
            .lines()
            .any(|l| l.to_ascii_lowercase().starts_with("bcc:")),
        "{headers}"
    );
    assert!(
        !f.smtp_recipients().iter().any(|r| r == "x@evil.test"),
        "{:?}",
        f.smtp_recipients()
    );
}

#[test]
fn read_only_config_and_environment_refuse_send_without_connecting() {
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("read_only = true", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--subject",
            "Hi",
            "--body",
            "x",
            "--json",
        ],
        &password_env(),
    );
    json_error(&out, 7, "refused");
    assert_eq!(f.smtp_connections(), 0);

    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--subject",
            "Hi",
            "--body",
            "x",
            "--json",
        ],
        &[("MAILCTL_PASSWORD", PASSWORD), ("MAILCTL_READONLY", "1")],
    );
    json_error(&out, 7, "refused");
    assert_eq!(f.smtp_connections(), 0);
}

#[test]
fn allowed_recipients_cover_to_and_bcc() {
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("allowed_recipients = [\"@example.com\"]", "");

    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "someone@evil.test",
            "--subject",
            "Hi",
            "--body",
            "x",
            "--json",
        ],
        &password_env(),
    );
    json_error(&out, 7, "refused");
    assert_eq!(f.smtp_connections(), 0);

    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--subject",
            "Hi",
            "--body",
            "x",
            "--json",
        ],
        &password_env(),
    );
    json_ok(&out);
    assert_eq!(f.smtp_connections(), 1);

    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--bcc",
            "secret@evil.test",
            "--subject",
            "Hi",
            "--body",
            "x",
            "--json",
        ],
        &password_env(),
    );
    json_error(&out, 7, "refused");
    assert_eq!(f.smtp_connections(), 1);
}

#[test]
fn dry_run_previews_the_message_without_a_password_or_connection() {
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--dry-run",
            "--to",
            "a@example.com",
            "--cc",
            "c@example.com",
            "--bcc",
            "b@example.com",
            "--subject",
            "Hi",
            "--body",
            "hello",
            "--json",
        ],
        &[NO_KEYCHAIN],
    );
    let v = json_ok(&out);
    assert_eq!(v["sent"], false);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["from"], USER);
    assert_eq!(v["to"], serde_json::json!(["a@example.com"]));
    assert_eq!(v["cc"], serde_json::json!(["c@example.com"]));
    assert_eq!(v["bcc"], serde_json::json!(["b@example.com"]));
    assert_eq!(v["subject"], "Hi");
    assert_eq!(v["body"], "hello");
    assert_eq!(f.smtp_connections(), 0);

    f.write_config("allowed_recipients = [\"@example.com\"]", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--dry-run",
            "--to",
            "a@evil.test",
            "--subject",
            "Hi",
            "--body",
            "hello",
            "--json",
        ],
        &[NO_KEYCHAIN],
    );
    json_error(&out, 7, "refused");
    assert_eq!(f.smtp_connections(), 0);

    f.write_config("read_only = true", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--dry-run",
            "--to",
            "a@example.com",
            "--subject",
            "Hi",
            "--body",
            "hello",
            "--json",
        ],
        &[NO_KEYCHAIN],
    );
    json_error(&out, 7, "refused");
    assert_eq!(f.smtp_connections(), 0);
}

#[test]
fn closing_the_output_pipe_exits_cleanly_without_a_panic() {
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    let config = f.config.to_string_lossy().into_owned();

    let mut child = Command::new(env!("CARGO_BIN_EXE_mailctl"))
        .env_clear()
        .env("MAILCTL_CONFIG", config)
        .env("MAILCTL_PASSWORD", PASSWORD)
        .args(["read", "30", "--body-limit", "0", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut first = [0u8; 1];
    assert_eq!(stdout.read(&mut first).unwrap(), 1);
    drop(stdout);

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("mailctl did not exit after the pipe closed");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let output = child.wait_with_output().unwrap();
    assert_eq!(status.code(), Some(0), "status: {status}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panic"), "panic on stderr: {stderr}");
}

#[test]
fn a_search_from_a_fixed_template_ignores_blank_terms() {
    // A harness that fills every parameter of a command template passes the
    // terms the caller left out as empty strings. They must not become
    // criteria that match nothing (or everything).
    let mut f = Fixture::new();
    f.start_imap();
    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "search",
            "--mailbox",
            "INBOX",
            "--limit",
            "5",
            "--subject",
            "",
            "--from",
            "emile@example.com",
            "--body",
            "",
            "--since",
            "",
            "--json",
        ],
        &password_env(),
    );
    let v = json_ok(&out);
    assert!(!v.as_array().unwrap().is_empty(), "{v}");
    let searches: Vec<String> = f
        .imap_commands()
        .into_iter()
        .filter(|c| c.to_ascii_uppercase().starts_with("UID SEARCH"))
        .collect();
    assert_eq!(searches.len(), 1, "{searches:?}");
    assert!(searches[0].contains("FROM"), "{searches:?}");
    assert!(
        !searches[0].contains("SUBJECT")
            && !searches[0].contains("BODY")
            && !searches[0].contains("SINCE"),
        "{searches:?}"
    );

    // All terms blank is still a usage error, not a search of everything.
    let none = run_f(
        &f,
        &["search", "--subject", "", "--from", "", "--json"],
        &password_env(),
    );
    json_error(&none, 2, "usage");
}

#[test]
fn a_body_or_subject_that_starts_with_a_hyphen_is_accepted() {
    // A bullet list is a normal body, and clap rejects a value that starts
    // with a hyphen unless the option allows it.
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "a@example.com",
            "--subject",
            "-urgent- follow up",
            "--body",
            "- first point\n- second point",
            "--json",
        ],
        &password_env(),
    );
    json_ok(&out);
    let data = f.smtp_data();
    assert!(data.contains("- first point"), "{data}");
    assert!(data.contains("- second point"), "{data}");
    assert!(data.contains("-urgent- follow up"), "{data}");
}

#[test]
fn a_usage_error_message_has_no_duplicated_error_prefix() {
    let f = Fixture::new();
    f.write_config("", "");
    let out = run_f(
        &f,
        &["send", "--to", "a@example.com", "--json"],
        &password_env(),
    );
    let v = json_error(&out, 2, "usage");
    let message = v["error"].as_str().unwrap();
    assert!(!message.starts_with("error:"), "{message}");
    assert!(message.contains("--subject"), "{message}");
}

#[test]
fn a_blank_search_is_a_usage_error_before_any_password_lookup() {
    // No password anywhere: the arguments are still checked first, so the
    // caller learns what is wrong with the call rather than about credentials.
    let f = Fixture::new();
    f.write_config("", "");
    let out = run_f(
        &f,
        &["search", "--subject", "", "--from", "", "--json"],
        &[(
            "MAILCTL_KEYCHAIN_SERVICE",
            "mailctl-test-service-that-does-not-exist",
        )],
    );
    json_error(&out, 2, "usage");
}

/// Run mailctl with a stdin pipe that stays open, and wait up to `deadline` for
/// it to exit. Returns the exit code (None if it had to be killed) and the
/// elapsed time. This is the shape of an agent harness that never closes stdin.
fn run_with_open_stdin(
    args: &[&str],
    env: &[(&str, &str)],
    deadline: Duration,
) -> (Option<i32>, Duration) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mailctl"));
    cmd.env_clear()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn mailctl");
    let _held_open = child.stdin.take();
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status.code(), start.elapsed());
        }
        if start.elapsed() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (None, start.elapsed());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_timeout_ends_the_process_even_when_stdin_is_never_closed() {
    // `--body -` waits for stdin to close. The timeout must end the process at
    // once instead of waiting for a pipe the caller will never close.
    let f = Fixture::new();
    f.write_config("", "");
    let config = f.config.to_string_lossy().into_owned();
    let (code, elapsed) = run_with_open_stdin(
        &[
            "send",
            "--json",
            "--timeout",
            "1",
            "--to",
            "a@example.com",
            "--subject",
            "s",
            "--body",
            "-",
        ],
        &[("MAILCTL_CONFIG", &config), ("MAILCTL_PASSWORD", "pw")],
        Duration::from_secs(8),
    );
    assert_eq!(code, Some(8), "hung for {elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[test]
fn read_only_refuses_before_reading_stdin() {
    let f = Fixture::new();
    f.write_config("read_only = true", "");
    let config = f.config.to_string_lossy().into_owned();
    let (code, elapsed) = run_with_open_stdin(
        &[
            "send",
            "--json",
            "--to",
            "a@example.com",
            "--subject",
            "s",
            "--body",
            "-",
        ],
        &[("MAILCTL_CONFIG", &config), ("MAILCTL_PASSWORD", "pw")],
        Duration::from_secs(8),
    );
    assert_eq!(code, Some(7), "waited for stdin for {elapsed:?}");
}

#[test]
fn a_send_timeout_says_the_outcome_is_unknown() {
    // The SMTP server accepts the connection and never answers, so the send
    // has started when the timeout fires: the caller must be told not to retry
    // blindly.
    let mut f = Fixture::new();
    let hung = Listener::start(|_stream, stop| {
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(20));
        }
    });
    f.smtp_port = hung.port();
    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--json",
            "--timeout",
            "1",
            "--to",
            "a@example.com",
            "--subject",
            "s",
            "--body",
            "b",
        ],
        &password_env(),
    );
    let v = json_error(&out, 8, "timeout");
    let message = v["error"].as_str().unwrap();
    assert!(message.contains("may or may not"), "{message}");
    assert!(message.contains("Sent folder"), "{message}");
}

#[test]
fn a_comma_inside_a_quoted_display_name_does_not_split_the_recipient() {
    let mut f = Fixture::new();
    f.start_smtp();
    f.write_config("", "");
    let out = run_f(
        &f,
        &[
            "send",
            "--to",
            "\"Doe, John\" <john@example.com>, other@example.com",
            "--subject",
            "s",
            "--body",
            "b",
            "--json",
        ],
        &password_env(),
    );
    json_ok(&out);
    let recipients = f.smtp_recipients();
    assert_eq!(recipients.len(), 2, "{recipients:?}");
    assert!(
        recipients.iter().any(|r| r.contains("john@example.com")),
        "{recipients:?}"
    );
}

#[test]
fn a_remote_plaintext_host_is_refused_before_the_password_is_looked_up() {
    let f = Fixture::new();
    f.write_raw_config(&format!(
        "user = \"{USER}\"\n[imap]\nhost = \"mail.example.com\"\n"
    ));
    let out = run_f(
        &f,
        &["folders", "--json"],
        &[(
            "MAILCTL_KEYCHAIN_SERVICE",
            "mailctl-test-service-that-does-not-exist",
        )],
    );
    let v = json_error(&out, 3, "config");
    assert!(v["error"].as_str().unwrap().contains("must use TLS"), "{v}");
}
