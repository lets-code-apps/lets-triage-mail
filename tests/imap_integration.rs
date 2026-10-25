//! Integration tests for the IMAP session against a real Dovecot server.
//!
//! All tests except the first are `#[ignore]`d because they need the Docker container from
//! `tests/dovecot/dovecot.conf`; CI starts it and runs `cargo test -- --ignored`. See
//! `tests/common/mod.rs` for how to run them by hand.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use common::{fresh_account, seed_inbox};
use lets_triage_mail::config::AccountConfig;
use lets_triage_mail::flags::{FlagColour, StoreChange};
use lets_triage_mail::imap::{ImapError, ImapSession, ImapSettings, TlsMode};
use lets_triage_mail::senders::{Skipped, harvest_known_senders};
use lets_triage_mail::store::Store;

/// This one needs no server: the refusal happens before any connection is made.
#[tokio::test]
async fn refuses_plain_text_to_a_remote_host() {
    let settings = ImapSettings {
        host: "imap.example.org".into(),
        port: 143,
        tls: TlsMode::PlainLoopbackOnly,
        username: "me".into(),
        password: "secret".into(),
    };
    let result = ImapSession::connect(&settings).await;
    assert!(matches!(result, Err(ImapError::InsecureTransport { .. })));
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn connects_and_reports_capabilities() {
    let mut session = ImapSession::connect(&fresh_account()).await.unwrap();
    let caps = session.capabilities().await.unwrap();
    assert!(caps.idle, "Dovecot supports IDLE: {:?}", caps.all);
    assert!(caps.uidplus, "Dovecot supports UIDPLUS: {:?}", caps.all);
    session.logout().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn wrong_password_is_a_login_error() {
    let mut settings = fresh_account();
    settings.password = "not-the-password".into();
    let result = ImapSession::connect(&settings).await;
    assert!(matches!(result, Err(ImapError::Login(_))));
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn select_inbox_reports_uids_and_allows_keywords() {
    let settings = fresh_account();
    seed_inbox(&settings, &["one", "two"]).await;

    let mut session = ImapSession::connect(&settings).await.unwrap();
    let info = session.select_inbox().await.unwrap();
    assert_eq!(info.exists, 2);
    assert_eq!(info.uid_next, 3);
    assert!(info.uid_validity > 0);
    assert!(
        info.keywords_allowed,
        "PERMANENTFLAGS: {:?}",
        info.permanent_flags
    );
    session.logout().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn resolves_inbox_and_sent_folders() {
    let mut session = ImapSession::connect(&fresh_account()).await.unwrap();
    let folders = session.resolve_folders().await.unwrap();
    assert_eq!(folders.inbox, "INBOX");
    assert_eq!(folders.sent.as_deref(), Some("Sent"));
    assert_eq!(folders.delimiter.as_deref(), Some("/"));
    session.logout().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn peek_fetches_do_not_mark_mail_as_read() {
    let settings = fresh_account();
    seed_inbox(&settings, &["peek-me"]).await;

    let mut session = ImapSession::connect(&settings).await.unwrap();
    session.select_inbox().await.unwrap();

    let body = session.fetch_body_peek(1).await.unwrap().expect("body");
    assert!(String::from_utf8_lossy(&body).contains("Subject: peek-me"));

    let headers = session
        .fetch_headers_peek(1)
        .await
        .unwrap()
        .expect("headers");
    let headers = String::from_utf8_lossy(&headers);
    assert!(headers.contains("Subject: peek-me"));
    assert!(!headers.contains("Hello from the integration test"));

    let meta = session.fetch_meta(&[1]).await.unwrap();
    assert_eq!(meta.len(), 1);
    assert_eq!(meta[0].uid, 1);
    assert!(meta[0].size.is_some_and(|size| size > 0));
    assert!(
        !meta[0].flags.iter().any(|f| f == "\\Seen"),
        "flags after PEEK: {:?}",
        meta[0].flags
    );
    session.logout().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn store_flags_sets_a_colour_and_replaces_stale_bits() {
    let settings = fresh_account();
    seed_inbox(&settings, &["colour-me"]).await;

    let mut session = ImapSession::connect(&settings).await.unwrap();
    session.select_inbox().await.unwrap();

    // Orange = \Flagged + bit 0.
    let orange = FlagColour::Orange.store_change().unwrap();
    session.store_flags(1, &orange).await.unwrap();
    let flags = session.fetch_meta(&[1]).await.unwrap().remove(0).flags;
    assert_eq!(
        FlagColour::from_flags(&as_strs(&flags)),
        Some(FlagColour::Orange)
    );

    // Green = \Flagged + bits 0 and 1. Switching must not leave anything stale behind.
    let green = FlagColour::Green.store_change().unwrap();
    session.store_flags(1, &green).await.unwrap();
    let flags = session.fetch_meta(&[1]).await.unwrap().remove(0).flags;
    assert_eq!(
        FlagColour::from_flags(&as_strs(&flags)),
        Some(FlagColour::Green)
    );

    // Yellow = bit 1 only: bit 0 from Green must be gone.
    let yellow = FlagColour::Yellow.store_change().unwrap();
    session.store_flags(1, &yellow).await.unwrap();
    let flags = session.fetch_meta(&[1]).await.unwrap().remove(0).flags;
    assert_eq!(
        FlagColour::from_flags(&as_strs(&flags)),
        Some(FlagColour::Yellow)
    );
    assert!(!flags.iter().any(|f| f == "$MailFlagBit0"), "{flags:?}");

    session.logout().await.unwrap();
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn store_flags_without_a_selected_mailbox_fails() {
    let mut session = ImapSession::connect(&fresh_account()).await.unwrap();
    let change = StoreChange {
        add: vec!["\\Flagged"],
        remove: vec![],
    };
    let result = session.store_flags(1, &change).await;
    assert!(matches!(result, Err(ImapError::NoMailboxSelected)));
}

/// Proves the chosen crate can IDLE through our TLS-agnostic stream type; task 3.5 builds the
/// real loop on this. With no new mail, the wait ends by timeout.
#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn idle_waits_and_times_out_without_new_mail() {
    let mut session = ImapSession::connect(&fresh_account()).await.unwrap();
    session.select_inbox().await.unwrap();

    let mut idle = session.into_inner().idle();
    idle.init().await.unwrap();
    let outcome = {
        let (wait, _stop) = idle.wait_with_timeout(Duration::from_secs(1));
        wait.await.unwrap()
    };
    assert!(matches!(outcome, IdleResponse::Timeout), "{outcome:?}");

    let mut raw = idle.done().await.unwrap();
    raw.logout().await.unwrap();
}

fn as_strs(flags: &[String]) -> Vec<&str> {
    flags.iter().map(String::as_str).collect()
}

/// APPEND messages to Sent with the given recipient headers (`To:` and optionally `Cc:`).
async fn seed_sent(settings: &ImapSettings, messages: &[(&str, Option<&str>)]) {
    let session = ImapSession::connect(settings).await.expect("connect");
    let mut raw = session.into_inner();
    for (to, cc) in messages {
        let cc_header = cc.map(|cc| format!("Cc: {cc}\r\n")).unwrap_or_default();
        let message = format!(
            "From: me@example.org\r\nTo: {to}\r\n{cc_header}Subject: sent\r\n\
             Date: Mon, 26 Oct 2026 09:00:00 +0000\r\n\r\nbody\r\n"
        );
        raw.append("Sent", None, None, message.into_bytes())
            .await
            .expect("append");
    }
    raw.logout().await.expect("logout");
}

/// A minimal account config pointing at the test server; `sent_folder` is left to detection.
fn account_config(settings: &ImapSettings) -> AccountConfig {
    let toml = format!(
        "name = \"test\"\nhost = \"{}\"\nusername = \"{}\"\n",
        settings.host, settings.username
    );
    toml::from_str(&toml).expect("account config")
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn harvests_known_senders_incrementally_from_sent() {
    let settings = fresh_account();
    let account = account_config(&settings);
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));

    seed_sent(
        &settings,
        &[
            ("Ann <Ann@Example.org>", Some("bob@example.org")),
            (
                "ann@example.org, Team: carol@example.org, dave@example.org;",
                None,
            ),
        ],
    )
    .await;

    let mut session = ImapSession::connect(&settings).await.unwrap();
    let first = harvest_known_senders(&mut session, &store, &account, true)
        .await
        .unwrap();
    assert_eq!(first.folder.as_deref(), Some("Sent"));
    assert_eq!(first.messages, 2);
    assert_eq!(first.new_senders, 4); // ann, bob, carol, dave
    {
        let store = store.lock().unwrap();
        for address in ["ann@example.org", "bob@example.org", "carol@example.org"] {
            assert!(store.is_known_sender(&account.name, address).unwrap());
        }
        assert!(
            !store
                .is_known_sender(&account.name, "eve@example.org")
                .unwrap()
        );
    }

    // Nothing new: no batches at all.
    let idle = harvest_known_senders(&mut session, &store, &account, true)
        .await
        .unwrap();
    assert_eq!((idle.batches, idle.messages, idle.new_senders), (0, 0, 0));

    // One more message: only that UID is read, and only its new address is added.
    seed_sent(&settings, &[("ann@example.org, eve@example.org", None)]).await;
    let second = harvest_known_senders(&mut session, &store, &account, true)
        .await
        .unwrap();
    assert_eq!(second.messages, 1);
    assert_eq!(second.new_senders, 1);
    assert!(
        store
            .lock()
            .unwrap()
            .is_known_sender(&account.name, "eve@example.org")
            .unwrap()
    );

    // Disabled by policy: does nothing.
    let off = harvest_known_senders(&mut session, &store, &account, false)
        .await
        .unwrap();
    assert_eq!(off.skipped, Some(Skipped::Disabled));
    session.logout().await.unwrap();
}

/// The `check` probes against a real server: no failures, and the numbers come from the
/// mailbox. (A fresh Dovecot user may have no Sent folder, so a warning is allowed.)
#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn check_probe_passes_against_dovecot() {
    use lets_triage_mail::check::{Status, probe_imap};

    let settings = fresh_account();
    seed_inbox(&settings, &["one", "two"]).await;

    let items = probe_imap("Account test", &settings, "INBOX", "").await;
    assert!(items.iter().all(|i| i.status != Status::Fail), "{items:#?}");
    let text = lets_triage_mail::check::format_items(&items);
    assert!(text.contains("2 messages"), "{text}");
    assert!(text.contains("custom keywords allowed"), "{text}");
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn check_probe_reports_a_wrong_password_with_a_hint() {
    use lets_triage_mail::check::{Status, probe_imap};

    let mut settings = fresh_account();
    settings.password = "not-the-password".into();
    let items = probe_imap("Account test", &settings, "INBOX", "").await;
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0].status, Status::Fail);
    assert!(items[0].hint.as_deref().unwrap().contains("password"));
}
