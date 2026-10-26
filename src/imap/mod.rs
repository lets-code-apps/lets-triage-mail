//! IMAP access: connection settings, errors and the [`session::ImapSession`] wrapper.
//!
//! # Crate choice: `async-imap`
//!
//! We use [`async-imap`](https://crates.io/crates/async-imap) 0.12 with its `runtime-tokio`
//! feature. Reasons:
//!
//! - It is maintained (0.12.0 was released in September 2026) and used by Delta Chat.
//! - It speaks async with tokio, like the rest of the service, so one task per account is cheap
//!   and no blocking threads are needed.
//! - It has everything v1 needs: `UID FETCH` with the `PEEK` body form, `UID STORE` with
//!   arbitrary keywords, `LIST` with special-use attributes, `APPEND` (for the digest) and
//!   **IDLE** (`Session::idle` gives a `Handle` whose `wait_with_timeout` is what task 3.5 will
//!   use).
//! - It brings no TLS of its own: you hand it any stream that implements tokio's `AsyncRead` and
//!   `AsyncWrite`. We pass `tokio-rustls` (pure Rust, no OpenSSL) with the operating system's
//!   certificate store, so self-hosted servers with a private CA work once the CA is trusted
//!   in the system.
//!
//! The fallback (the synchronous `imap` crate on blocking threads) was not needed.
//!
//! # Design
//!
//! Logic that does not need a server (parsing capabilities, checking `PERMANENTFLAGS`, choosing
//! folders from `LIST` output, building `STORE` arguments) lives in plain functions that take
//! plain data. They are unit-tested without a server. The thin `async` methods that talk to the
//! server call them.
//!
//! # Hard rules enforced here
//!
//! - Messages are only fetched with the `PEEK` form, so nothing is marked as read. A unit test
//!   scans the source to make sure it stays that way.
//! - This module has no `EXPUNGE`, `COPY`, `MOVE` or delete commands. v1 only sets and clears
//!   flag keywords.
//! - The password is never logged, and plain text is refused for non-loopback hosts.

#[cfg(test)]
pub(crate) mod fake_server;
pub mod folders;
pub mod session;
pub mod utf7;
pub mod worker;

use std::fmt;
use std::net::IpAddr;

pub use folders::{ListedFolder, ResolvedFolders, resolve_folders};
pub use session::{
    AppendResult, EnvelopeAddresses, IdleEnd, ImapSession, MailboxInfo, MessageMeta,
    ServerCapabilities,
};
pub use utf7::decode_modified_utf7;

/// How to protect the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// TLS from the first byte (usually port 993). The default for real accounts.
    Implicit,
    /// Start in plain text, then upgrade with `STARTTLS` (usually port 143) before logging in.
    StartTls,
    /// No encryption at all. Only accepted for loopback hosts, which exists for tests.
    PlainLoopbackOnly,
}

/// Everything needed to open an IMAP connection.
///
/// The config module will produce one of these per account; it is defined here so the IMAP
/// code does not depend on it.
#[derive(Clone)]
pub struct ImapSettings {
    /// Server name, used for the TCP connection and for certificate checks.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// How the connection is secured.
    pub tls: TlsMode,
    /// Login name.
    pub username: String,
    /// The password itself. The caller reads it from the keychain or an environment variable.
    pub password: String,
}

// A hand-written `Debug` instead of `#[derive(Debug)]`, so the password can never end up in a
// log line by accident (for example through `tracing::debug!("{settings:?}")`).
impl fmt::Debug for ImapSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl ImapSettings {
    /// Refuse settings that would send the password in clear text over a network.
    pub fn validate(&self) -> Result<(), ImapError> {
        if self.tls == TlsMode::PlainLoopbackOnly && !is_loopback_host(&self.host) {
            return Err(ImapError::InsecureTransport {
                host: self.host.clone(),
            });
        }
        Ok(())
    }
}

/// True for `localhost` and loopback IP addresses (`127.0.0.0/8`, `::1`).
///
/// The one definition of "loopback" in the crate: the config validation uses it too.
pub fn is_loopback_host(host: &str) -> bool {
    // `[::1]` is how an IPv6 literal may be written in a URL-like form.
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Everything that can go wrong while talking to an IMAP server.
///
/// This is a library error type (`thiserror`): callers can match on the variants. The binary
/// wraps it into `anyhow::Error` at the edge.
#[derive(Debug, thiserror::Error)]
pub enum ImapError {
    /// Plain text was requested for a host that is not loopback.
    #[error("refusing to send the password in clear text to {host}; use TLS or STARTTLS")]
    InsecureTransport {
        /// The offending host.
        host: String,
    },
    /// The server did not accept `STARTTLS`.
    #[error("the server does not support STARTTLS; refusing to log in without encryption")]
    StartTlsUnsupported,
    /// The host name is not a valid TLS server name.
    #[error("{0:?} is not a valid server name for TLS")]
    InvalidServerName(String),
    /// No usable root certificates were found in the system certificate store.
    #[error("no trusted root certificates found in the system certificate store")]
    NoRootCertificates,
    /// TLS configuration or handshake failed.
    #[error("TLS error: {0}")]
    Tls(String),
    /// The TCP connection or the IO on it failed.
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    /// Connecting did not finish in time.
    #[error("timed out connecting to the IMAP server")]
    Timeout,
    /// The server rejected the login.
    #[error("login failed: {0}")]
    Login(#[source] async_imap::error::Error),
    /// Any other IMAP protocol error.
    #[error("IMAP error: {0}")]
    Imap(#[from] async_imap::error::Error),
    /// Flags were requested but the mailbox does not allow custom keywords.
    #[error("the mailbox does not allow custom keywords (PERMANENTFLAGS lacks \\*)")]
    KeywordsNotAllowed,
    /// `store_flags` was called before any mailbox was selected.
    #[error("no mailbox is selected")]
    NoMailboxSelected,
    /// A folder name that cannot be sent safely (empty, or with a line break or NUL).
    #[error("{0:?} is not a usable folder name")]
    InvalidFolderName(String),
    /// A command was given an empty list of UIDs.
    #[error("the UID list is empty")]
    EmptyUidSet,
    /// The server's answer lacked something we asked for.
    #[error("the server response is missing {0}")]
    MissingData(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(host: &str, tls: TlsMode) -> ImapSettings {
        ImapSettings {
            host: host.into(),
            port: 143,
            tls,
            username: "me".into(),
            password: "hunter2".into(),
        }
    }

    #[test]
    fn plain_text_is_allowed_for_loopback() {
        for host in ["127.0.0.1", "localhost", "LOCALHOST", "::1", "127.0.0.5"] {
            assert!(
                settings(host, TlsMode::PlainLoopbackOnly)
                    .validate()
                    .is_ok(),
                "{host}"
            );
        }
    }

    #[test]
    fn plain_text_is_refused_for_other_hosts() {
        for host in [
            "imap.example.org",
            "192.168.1.10",
            "10.0.0.1",
            "localhost.evil.org",
        ] {
            assert!(
                matches!(
                    settings(host, TlsMode::PlainLoopbackOnly).validate(),
                    Err(ImapError::InsecureTransport { .. })
                ),
                "{host}"
            );
        }
    }

    #[test]
    fn encrypted_modes_are_allowed_everywhere() {
        assert!(
            settings("imap.example.org", TlsMode::Implicit)
                .validate()
                .is_ok()
        );
        assert!(
            settings("imap.example.org", TlsMode::StartTls)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn debug_output_hides_the_password() {
        let text = format!("{:?}", settings("imap.example.org", TlsMode::Implicit));
        assert!(!text.contains("hunter2"));
        assert!(text.contains("redacted"));
    }
}
