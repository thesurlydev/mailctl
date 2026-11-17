//! Failure kinds and exit codes.
//!
//! A caller that drives `mailctl` from a script or an agent needs to tell "Bridge
//! is not running" from "wrong password" from "the policy refused this" without
//! parsing prose. Every failure therefore carries a [`Kind`]; the kind picks the
//! exit code, and with `--json` it is printed as the `kind` field of the error
//! object.

use std::fmt;

use serde::Serialize;

/// Why a command failed. The exit code is stable; see [`Kind::exit_code`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Anything not covered below, including unexpected protocol errors.
    Other,
    /// Bad arguments or a malformed value (an address, a date).
    Usage,
    /// The config file or password source is missing or invalid.
    Config,
    /// Nothing is listening at the IMAP or SMTP endpoint (Bridge not running).
    Unreachable,
    /// The server rejected the credentials.
    Auth,
    /// The mailbox or message does not exist.
    NotFound,
    /// The configured policy (`read_only`, `allowed_recipients`) refused the action.
    Refused,
    /// The command did not finish within the timeout.
    Timeout,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Other => "other",
            Kind::Usage => "usage",
            Kind::Config => "config",
            Kind::Unreachable => "unreachable",
            Kind::Auth => "auth",
            Kind::NotFound => "not_found",
            Kind::Refused => "refused",
            Kind::Timeout => "timeout",
        }
    }

    pub fn exit_code(self) -> u8 {
        match self {
            Kind::Other => 1,
            Kind::Usage => 2,
            Kind::Config => 3,
            Kind::Unreachable => 4,
            Kind::Auth => 5,
            Kind::NotFound => 6,
            Kind::Refused => 7,
            Kind::Timeout => 8,
        }
    }
}

/// An error tagged with its [`Kind`]. It travels inside an `anyhow::Error`, so
/// `.context(..)` added further up does not lose the kind.
#[derive(Debug)]
pub struct Failure {
    kind: Kind,
    source: anyhow::Error,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.source)
    }
}

impl std::error::Error for Failure {}

/// Tag an error with a kind.
pub fn fail(kind: Kind, err: impl Into<anyhow::Error>) -> anyhow::Error {
    anyhow::Error::new(Failure {
        kind,
        source: err.into(),
    })
}

/// The kind of an error, [`Kind::Other`] when it was never tagged.
pub fn kind_of(err: &anyhow::Error) -> Kind {
    err.downcast_ref::<Failure>()
        .map(|f| f.kind)
        .unwrap_or(Kind::Other)
}

/// True when the error is the reader closing the pipe (`mailctl ... | head`).
pub fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

/// What `--json` prints on stdout when a command fails.
#[derive(Serialize)]
pub struct ErrorBody<'a> {
    pub error: &'a str,
    pub kind: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn kind_survives_added_context() {
        let err = fail(Kind::Auth, anyhow!("bad password")).context("logging in");
        assert_eq!(kind_of(&err), Kind::Auth);
        assert_eq!(format!("{err:#}"), "logging in: bad password");
    }

    #[test]
    fn untagged_errors_are_other() {
        assert_eq!(kind_of(&anyhow!("boom")), Kind::Other);
    }

    #[test]
    fn exit_codes_are_distinct_and_stable() {
        let all = [
            (Kind::Other, 1, "other"),
            (Kind::Usage, 2, "usage"),
            (Kind::Config, 3, "config"),
            (Kind::Unreachable, 4, "unreachable"),
            (Kind::Auth, 5, "auth"),
            (Kind::NotFound, 6, "not_found"),
            (Kind::Refused, 7, "refused"),
            (Kind::Timeout, 8, "timeout"),
        ];
        for (kind, code, name) in all {
            assert_eq!(kind.exit_code(), code);
            assert_eq!(kind.as_str(), name);
        }
    }

    #[test]
    fn broken_pipe_is_recognized_through_context() {
        let io = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        let err = anyhow::Error::new(io).context("writing output");
        assert!(is_broken_pipe(&err));
        assert!(!is_broken_pipe(&anyhow!("other")));
    }
}
