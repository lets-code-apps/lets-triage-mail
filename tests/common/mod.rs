//! Helpers shared by the integration tests that need a real IMAP server.
//!
//! The server is a Dovecot container (see `tests/dovecot/dovecot.conf`). Plain `cargo test`
//! does not start it, so every test that uses it is `#[ignore]`d. To run them:
//!
//! ```text
//! docker run -d --name ltm-dovecot -p 1143:143 \
//!   -v "$PWD/tests/dovecot/dovecot.conf:/etc/dovecot/dovecot.conf:ro" \
//!   dovecot/dovecot:2.3.21.1
//! cargo test --test imap_integration -- --ignored
//! ```
//!
//! On Apple Silicon, turn off Docker Desktop's Rosetta emulation first: the image is amd64-only
//! and Dovecot crashes under Rosetta (see `docs/ARCHITECTURE.md`, Testing).
//!
//! `LTM_IMAP_TEST_HOST` and `LTM_IMAP_TEST_PORT` override the default `127.0.0.1:1143`.

use std::sync::atomic::{AtomicU32, Ordering};

use lets_triage_mail::imap::{ImapSession, ImapSettings, TlsMode};

/// Password the test Dovecot accepts for every user.
const PASSWORD: &str = "testpass";

/// Connection settings for a user that has never been used before.
///
/// The Dovecot test config creates a user on first login, so a unique name gives each test its
/// own empty mailbox. The process id keeps names unique across repeated runs against the same
/// container; the counter keeps them unique between tests in one run.
pub fn fresh_account() -> ImapSettings {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);

    ImapSettings {
        host: std::env::var("LTM_IMAP_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
        port: std::env::var("LTM_IMAP_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(1143),
        tls: TlsMode::PlainLoopbackOnly,
        username: format!("user-{}-{n}", std::process::id()),
        password: PASSWORD.to_string(),
    }
}

/// A minimal RFC 5322 message with a unique `Message-ID`.
pub fn sample_message(subject: &str) -> Vec<u8> {
    format!(
        "From: Alice <alice@example.org>\r\n\
         To: bob@example.org\r\n\
         Subject: {subject}\r\n\
         Message-ID: <{subject}@tests.example.org>\r\n\
         Date: Mon, 26 Oct 2026 09:00:00 +0000\r\n\
         \r\n\
         Hello from the integration test.\r\n"
    )
    .into_bytes()
}

/// Put messages into a folder with `APPEND` and return the session for further use.
///
/// `APPEND` is how the daily digest will be written too, so the tests double as practice for
/// that. `APPEND` is not part of [`ImapSession`]'s API because the triage code never needs it,
/// so we reach for the underlying `async_imap::Session` through `into_inner`.
pub async fn seed_inbox(settings: &ImapSettings, subjects: &[&str]) {
    let session = ImapSession::connect(settings).await.expect("connect");
    let mut raw = session.into_inner();
    for subject in subjects {
        raw.append("INBOX", None, None, sample_message(subject))
            .await
            .expect("append");
    }
    raw.logout().await.expect("logout");
}
