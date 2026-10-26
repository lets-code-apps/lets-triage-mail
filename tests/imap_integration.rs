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
use lets_triage_mail::config::{AccountConfig, Config};
use lets_triage_mail::decider::{DecideError, Decider, Evidence};
use lets_triage_mail::engine::{Category, Payment, Verdict};
use lets_triage_mail::flags::{FlagColour, StoreChange};
use lets_triage_mail::imap::worker::{
    WorkerError, WorkerTiming, backfill_account, run_account_with_timing,
};
use lets_triage_mail::imap::{ImapError, ImapSession, ImapSettings, TlsMode};
use lets_triage_mail::senders::{Skipped, harvest_known_senders};
use lets_triage_mail::store::{MailboxState, MessageKey, Store};
use tokio_util::sync::CancellationToken;

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

    let items = probe_imap("Account test", &settings, "INBOX", "", true).await;
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
    let items = probe_imap("Account test", &settings, "INBOX", "", true).await;
    assert_eq!(items.len(), 1, "{items:#?}");
    assert_eq!(items[0].status, Status::Fail);
    assert!(items[0].hint.as_deref().unwrap().contains("password"));
}

// ---------------------------------------------------------------------------------------------
// The account worker (task 3.5), in dry-run
// ---------------------------------------------------------------------------------------------

/// A decider that needs no model: it either answers "personal, needs a reply" (which would get
/// the Orange flag if flagging were on) or fails. That makes "nothing changed on the server"
/// a meaningful check.
struct FakeDecider {
    fail: bool,
}

impl Decider for FakeDecider {
    async fn decide(&self, _evidence: &Evidence) -> Result<Verdict, DecideError> {
        if self.fail {
            return Err(DecideError::InvalidVerdict("fake model failure".into()));
        }
        Ok(Verdict {
            category: Category::Personal,
            needs_reply: true,
            deadline: None,
            payment: Payment::NotApplicable,
            confidence: 0.95,
            reason: "fake".into(),
        })
    }
}

/// The account config for the worker. The password comes from `password_command`, so the test
/// does not have to change the process environment.
fn worker_account(settings: &ImapSettings) -> AccountConfig {
    worker_account_with(settings, "")
}

/// Same, with extra account settings such as `dry_run = false`.
fn worker_account_with(settings: &ImapSettings, extra: &str) -> AccountConfig {
    let toml = format!(
        "name = \"test\"\nhost = \"{}\"\nport = {}\nusername = \"{}\"\ntls = \"none\"\n\
         password_command = \"echo testpass\"\n{extra}",
        settings.host, settings.port, settings.username
    );
    toml::from_str(&toml).expect("account config")
}

/// A running worker that can be stopped. Dry-run is the default in `Config`.
struct RunningWorker {
    store: Arc<Mutex<Store>>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), WorkerError>>,
}

fn start_worker(settings: &ImapSettings, fail: bool, idle_renew: Duration) -> RunningWorker {
    // `[policy] dry_run` keeps its default (on) and the account has no override.
    start_worker_with(settings, fail, idle_renew, "")
}

/// A worker for an account with extra settings, for example `dry_run = false` to go live on
/// that account only. `[policy] dry_run` stays at its default (on), which is the point of the
/// per-account override. The clear pass runs every second so the tests need not wait long.
fn start_worker_with(
    settings: &ImapSettings,
    fail: bool,
    idle_renew: Duration,
    account_extra: &str,
) -> RunningWorker {
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let shutdown = CancellationToken::new();
    let (account, config) = (
        worker_account_with(settings, account_extra),
        Config::default(),
    );
    assert!(config.policy.dry_run);
    let timing = WorkerTiming {
        idle_renew,
        poll_interval: Duration::from_secs(1),
        clear_interval: Duration::from_secs(1),
        ..WorkerTiming::default()
    };
    let task = tokio::spawn({
        let (store, shutdown) = (Arc::clone(&store), shutdown.clone());
        async move {
            run_account_with_timing(
                &account,
                &config,
                Arc::new(FakeDecider { fail }),
                store,
                shutdown,
                timing,
            )
            .await
        }
    });
    RunningWorker {
        store,
        shutdown,
        task,
    }
}

impl RunningWorker {
    /// Cancel and check that the worker ends cleanly and promptly.
    async fn stop(self) {
        self.shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .expect("the worker stops within 10 seconds")
            .expect("the worker task did not panic");
        result.expect("the worker ends without an error");
    }

    /// The saved inbox position, once the worker has written it.
    fn state(&self) -> Option<MailboxState> {
        self.store
            .lock()
            .unwrap()
            .mailbox_state("test", "INBOX")
            .unwrap()
    }

    fn is_processed(&self, uidvalidity: u32, uid: u32) -> bool {
        self.store
            .lock()
            .unwrap()
            .is_processed(&inbox_key(uidvalidity, uid))
            .unwrap()
    }
}

/// Poll `condition` every 100 ms until it holds or `timeout` passes.
async fn wait_until(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + timeout;
    while !condition() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn inbox_key(uidvalidity: u32, uid: u32) -> MessageKey {
    MessageKey {
        account: "test".into(),
        mailbox: "INBOX".into(),
        uidvalidity,
        uid,
    }
}

/// The flags the server reports for one inbox message, read with a fresh connection.
async fn server_flags(settings: &ImapSettings, uid: u32) -> Vec<String> {
    let mut session = ImapSession::connect(settings).await.unwrap();
    session.select_inbox().await.unwrap();
    let metas = session.fetch_meta(&[uid]).await.unwrap();
    session.logout().await.unwrap();
    metas.into_iter().next().expect("message exists").flags
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn worker_first_run_ignores_the_existing_history() {
    let settings = fresh_account();
    seed_inbox(&settings, &["old-one", "old-two"]).await;

    let worker = start_worker(&settings, false, Duration::from_secs(1));
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;
    // Give it a few IDLE rounds to prove it does not go back for the history.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let state = worker.state().unwrap();
    assert_eq!(state.last_uid, 2, "UIDNEXT - 1");
    assert!(!worker.is_processed(state.uidvalidity, 1));
    assert!(!worker.is_processed(state.uidvalidity, 2));
    worker.stop().await;
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn worker_processes_new_mail_and_changes_nothing_on_the_server() {
    let settings = fresh_account();
    seed_inbox(&settings, &["old-one"]).await;

    let worker = start_worker(&settings, false, Duration::from_secs(1));
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;
    let validity = worker.state().unwrap().uidvalidity;

    seed_inbox(&settings, &["fresh-mail"]).await; // UID 2
    wait_until(
        Duration::from_secs(15),
        "the new message to be processed",
        || worker.is_processed(validity, 2),
    )
    .await;
    assert_eq!(worker.state().unwrap().last_uid, 2);

    // The fake model said "reply needed", so a real run would have flagged it orange.
    // Dry-run: no flags at all (so not \Seen either: it is still unread).
    let flags = server_flags(&settings, 2).await;
    assert!(flags.is_empty(), "dry-run must not change flags: {flags:?}");
    // The old message was never touched or processed.
    assert!(!worker.is_processed(validity, 1));
    worker.stop().await;
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn worker_wakes_from_idle_when_mail_arrives() {
    let settings = fresh_account();
    // A long IDLE period: processing within seconds can only come from the server's push.
    let worker = start_worker(&settings, false, Duration::from_secs(60));
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;
    let validity = worker.state().unwrap().uidvalidity;
    tokio::time::sleep(Duration::from_secs(1)).await; // let it settle into IDLE

    seed_inbox(&settings, &["while-idle"]).await;
    wait_until(Duration::from_secs(8), "the IDLE wake-up", || {
        worker.is_processed(validity, 1)
    })
    .await;
    worker.stop().await;
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn worker_queues_a_retry_when_the_model_fails() {
    let settings = fresh_account();
    let worker = start_worker(&settings, true, Duration::from_secs(1));
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;
    let validity = worker.state().unwrap().uidvalidity;

    seed_inbox(&settings, &["model-down"]).await; // UID 1
    let queued = |worker: &RunningWorker| {
        worker
            .store
            .lock()
            .unwrap()
            .due_retries("test", i64::MAX, 10)
            .unwrap()
    };
    wait_until(
        Duration::from_secs(15),
        "the message to reach the retry queue",
        || !queued(&worker).is_empty(),
    )
    .await;

    let items = queued(&worker);
    assert_eq!(items[0].key, inbox_key(validity, 1));
    assert_eq!(items[0].attempts, 1);
    // No guess was recorded, and the message on the server is untouched.
    assert!(!worker.is_processed(validity, 1));
    assert!(server_flags(&settings, 1).await.is_empty());
    worker.stop().await;
}

// ---------------------------------------------------------------------------------------------
// Flagging (tasks 4.1 and 4.2)
// ---------------------------------------------------------------------------------------------

/// APPEND one message to `folder`, with optional flags such as `(\\Flagged)`.
async fn append_raw(settings: &ImapSettings, folder: &str, flags: Option<&str>, message: Vec<u8>) {
    let session = ImapSession::connect(settings).await.expect("connect");
    let mut raw = session.into_inner();
    raw.append(folder, flags, None, message)
        .await
        .expect("append");
    raw.logout().await.expect("logout");
}

/// The user's reply, as it would sit in Sent: it names the answered message in `In-Reply-To`.
async fn append_reply_in_sent(settings: &ImapSettings, answered_message_id: &str) {
    let message = format!(
        "From: me@example.org\r\nTo: alice@example.org\r\nSubject: Re: hello\r\n\
         Message-ID: <my-reply@tests.example.org>\r\n\
         In-Reply-To: <{answered_message_id}>\r\nReferences: <{answered_message_id}>\r\n\
         Date: Mon, 26 Oct 2026 10:00:00 +0000\r\n\r\nSure.\r\n"
    );
    append_raw(settings, "Sent", None, message.into_bytes()).await;
}

fn is_orange(flags: &[String]) -> bool {
    FlagColour::from_flags(&as_strs(flags)) == Some(FlagColour::Orange)
}

fn owned_uids(worker: &RunningWorker) -> Vec<u32> {
    worker
        .store
        .lock()
        .unwrap()
        .owned_flags("test", "INBOX")
        .unwrap()
        .iter()
        .map(|flag| flag.key.uid)
        .collect()
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn a_live_account_flags_new_mail_orange_and_records_ownership() {
    let settings = fresh_account();
    seed_inbox(&settings, &["old-one"]).await;

    // Live on this account only: the policy stays in dry-run.
    let worker = start_worker_with(
        &settings,
        false,
        Duration::from_secs(1),
        "dry_run = false\n",
    );
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;
    let validity = worker.state().unwrap().uidvalidity;

    seed_inbox(&settings, &["needs-an-answer"]).await; // UID 2
    wait_until(Duration::from_secs(15), "the message to be flagged", || {
        worker.is_processed(validity, 2)
    })
    .await;

    let flags = server_flags(&settings, 2).await;
    assert!(is_orange(&flags), "flags on the server: {flags:?}");
    assert_eq!(owned_uids(&worker), [2]);
    // The old message is history: not flagged.
    assert!(server_flags(&settings, 1).await.is_empty());
    worker.stop().await;
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn a_message_the_user_already_flagged_is_left_untouched() {
    let settings = fresh_account();
    let worker = start_worker_with(
        &settings,
        false,
        Duration::from_secs(1),
        "dry_run = false\n",
    );
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;

    // Arrives already flagged (the user's red flag: \Flagged and no colour bits).
    append_raw(
        &settings,
        "INBOX",
        Some("(\\Flagged)"),
        common::sample_message("user-flagged"),
    )
    .await;
    // A second, plain message proves the worker got past the first one.
    seed_inbox(&settings, &["plain"]).await;
    let validity = worker.state().unwrap().uidvalidity;
    wait_until(Duration::from_secs(15), "the plain message", || {
        worker.is_processed(validity, 2)
    })
    .await;

    assert_eq!(server_flags(&settings, 1).await, ["\\Flagged"]);
    assert_eq!(owned_uids(&worker), [2], "only the plain message is ours");
    worker.stop().await;
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn a_dry_run_account_and_a_flagging_off_account_store_nothing() {
    for extra in ["dry_run = true\n", "dry_run = false\nflagging = false\n"] {
        let settings = fresh_account();
        let worker = start_worker_with(&settings, false, Duration::from_secs(1), extra);
        wait_until(Duration::from_secs(10), "the first-run position", || {
            worker.state().is_some()
        })
        .await;
        let validity = worker.state().unwrap().uidvalidity;

        seed_inbox(&settings, &["would-be-flagged"]).await;
        wait_until(
            Duration::from_secs(15),
            "the message to be processed",
            || worker.is_processed(validity, 1),
        )
        .await;
        // Give a clear pass a chance too.
        tokio::time::sleep(Duration::from_secs(2)).await;

        let flags = server_flags(&settings, 1).await;
        assert!(flags.is_empty(), "{extra:?} must not store: {flags:?}");
        assert!(owned_uids(&worker).is_empty());
        worker.stop().await;
    }
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn a_colour_the_user_changed_becomes_user_owned_and_is_never_cleared() {
    let settings = fresh_account();
    let worker = start_worker_with(
        &settings,
        false,
        Duration::from_secs(1),
        "dry_run = false\n",
    );
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;
    let validity = worker.state().unwrap().uidvalidity;

    seed_inbox(&settings, &["recoloured"]).await; // UID 1, Message-ID <recoloured@tests...>
    wait_until(Duration::from_secs(15), "the message to be flagged", || {
        owned_uids(&worker) == [1]
    })
    .await;

    // The user changes the colour from orange to green in their mail client.
    let mut session = ImapSession::connect(&settings).await.unwrap();
    session.select_inbox().await.unwrap();
    session
        .store_flags(1, &FlagColour::Green.store_change().unwrap())
        .await
        .unwrap();
    session.logout().await.unwrap();

    wait_until(
        Duration::from_secs(15),
        "the flag to become user-owned",
        || {
            worker
                .store
                .lock()
                .unwrap()
                .is_user_owned(&inbox_key(validity, 1))
                .unwrap()
        },
    )
    .await;

    // Even when the user replies, the service must not touch the green flag.
    append_reply_in_sent(&settings, "recoloured@tests.example.org").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let flags = server_flags(&settings, 1).await;
    assert_eq!(
        FlagColour::from_flags(&as_strs(&flags)),
        Some(FlagColour::Green),
        "{flags:?}"
    );
    worker.stop().await;
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn a_reply_in_sent_clears_reply_needed() {
    let settings = fresh_account();
    let worker = start_worker_with(
        &settings,
        false,
        Duration::from_secs(1),
        "dry_run = false\n",
    );
    wait_until(Duration::from_secs(10), "the first-run position", || {
        worker.state().is_some()
    })
    .await;

    seed_inbox(&settings, &["please-answer"]).await; // UID 1
    wait_until(Duration::from_secs(15), "the message to be flagged", || {
        owned_uids(&worker) == [1]
    })
    .await;
    assert!(is_orange(&server_flags(&settings, 1).await));

    // Some time passes with no reply: the flag stays.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(is_orange(&server_flags(&settings, 1).await));

    // The user answers from another device: the reply lands in Sent.
    append_reply_in_sent(&settings, "please-answer@tests.example.org").await;

    wait_until(Duration::from_secs(20), "the flag to be cleared", || {
        owned_uids(&worker).is_empty()
    })
    .await;
    let flags = server_flags(&settings, 1).await;
    assert!(
        !flags.iter().any(|f| f.eq_ignore_ascii_case("\\Flagged")),
        "still flagged: {flags:?}"
    );
    assert!(
        !flags.iter().any(|f| f.starts_with("$MailFlagBit")),
        "colour bits left behind: {flags:?}"
    );
    worker.stop().await;
}

/// The `backfill` pipeline over mail that is already in the inbox.
#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn backfill_processes_recent_mail_and_leaves_the_workers_position_alone() {
    use jiff::ToSpan;

    let settings = fresh_account();
    seed_inbox(&settings, &["one", "two", "three"]).await;

    // The worker has been here before and stands at UID 3 of this mailbox.
    let mut session = ImapSession::connect(&settings).await.unwrap();
    let validity = session.select_inbox().await.unwrap().uid_validity;
    session.logout().await.unwrap();
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    store
        .lock()
        .unwrap()
        .set_mailbox_state("test", "INBOX", validity, 3)
        .unwrap();

    let account = worker_account_with(&settings, "dry_run = false\n");
    let config = Config::default();
    let since = jiff::Zoned::now().date().checked_sub(1.day()).unwrap();
    let run = |store: Arc<Mutex<Store>>| {
        backfill_account(
            &account,
            &config,
            Arc::new(FakeDecider { fail: false }),
            store,
            CancellationToken::new(),
            since,
        )
    };

    let report = run(Arc::clone(&store)).await.unwrap();
    assert_eq!((report.found, report.processed), (3, 3), "{report:?}");
    for uid in 1..=3 {
        assert!(
            store
                .lock()
                .unwrap()
                .is_processed(&inbox_key(validity, uid))
                .unwrap(),
            "UID {uid}"
        );
        assert!(is_orange(&server_flags(&settings, uid).await), "UID {uid}");
    }
    // The cursor did not move, backwards or forwards.
    let state = store
        .lock()
        .unwrap()
        .mailbox_state("test", "INBOX")
        .unwrap()
        .unwrap();
    assert_eq!((state.uidvalidity, state.last_uid), (validity, 3));

    // A second run finds everything handled already.
    let again = run(Arc::clone(&store)).await.unwrap();
    assert_eq!(
        (again.found, again.processed, again.skipped),
        (3, 0, 3),
        "{again:?}"
    );
}

#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn backfill_on_a_dry_run_account_changes_nothing_on_the_server() {
    use jiff::ToSpan;

    let settings = fresh_account();
    seed_inbox(&settings, &["one", "two"]).await;
    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    // No override: the account follows `[policy] dry_run`, which is on.
    let account = worker_account(&settings);
    let config = Config::default();
    let since = jiff::Zoned::now().date().checked_sub(1.day()).unwrap();

    let report = backfill_account(
        &account,
        &config,
        Arc::new(FakeDecider { fail: false }),
        Arc::clone(&store),
        CancellationToken::new(),
        since,
    )
    .await
    .unwrap();

    assert_eq!(report.processed, 2, "{report:?}");
    for uid in 1..=2 {
        assert!(server_flags(&settings, uid).await.is_empty());
    }
    assert!(
        store
            .lock()
            .unwrap()
            .owned_flags("test", "INBOX")
            .unwrap()
            .is_empty()
    );
}

/// The supervisor, driven directly (the CLI builds a real `LlmDecider`, which cannot be faked
/// from outside the binary). Two accounts: one good, one with a wrong password. The bad one
/// fails its login and waits to be restarted; the good one must keep working, and shutdown must
/// stop both well inside the grace period.
#[tokio::test]
#[ignore = "needs the Dovecot test container"]
async fn supervisor_keeps_a_good_account_running_next_to_a_rejected_login() {
    use lets_triage_mail::supervisor::{SupervisorTiming, run_service};

    let (good, bad) = (fresh_account(), fresh_account());
    let mut wrong_password = worker_account(&bad);
    wrong_password.name = "bad".into();
    wrong_password.password_command = "echo wrong-password".into();
    let config = Config {
        accounts: vec![worker_account(&good), wrong_password],
        ..Config::default()
    };
    assert!(config.policy.dry_run);

    let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
    let shutdown = CancellationToken::new();
    let service = tokio::spawn(run_service(
        Arc::new(config),
        Arc::new(FakeDecider { fail: false }),
        Arc::clone(&store),
        shutdown.clone(),
        SupervisorTiming::default(),
    ));

    wait_until(
        Duration::from_secs(10),
        "the good account's position",
        || {
            store
                .lock()
                .unwrap()
                .mailbox_state("test", "INBOX")
                .unwrap()
                .is_some()
        },
    )
    .await;

    shutdown.cancel();
    let report = tokio::time::timeout(Duration::from_secs(10), service)
        .await
        .expect("the service stops within 10 seconds")
        .expect("the service task did not panic");
    assert_eq!(report.accounts, 2);
    assert_eq!(
        report.aborted, 0,
        "everything stopped inside the grace period"
    );
    assert!(!report.every_account_failed());
}
