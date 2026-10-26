# Architecture (Rust)

Nothing is implemented yet. This is the starting plan; verify crate versions and APIs before use.

## Candidate crates

| Need | Crate | Notes |
|---|---|---|
| async runtime | `tokio` | multi-thread runtime, one task per account |
| IMAP | `async-imap` (+ `tokio-rustls` / `async-native-tls`) | IDLE support exists; check MOVE, UIDPLUS, keyword STORE ergonomics. Alternative: `imap` (sync) on blocking threads. |
| MIME parsing | `mail-parser` (Stalwart) | handles encodings, multipart, headers well |
| HTML → text | `html2text` or `scraper` | drop script/style; count links |
| HTTP client (LLM) | `reqwest` (rustls) | OpenAI-compatible `/v1/chat/completions` |
| JSON | `serde`, `serde_json` | typed verdict struct; schema via `schemars` |
| config | `serde` + `toml` | |
| storage | `rusqlite` (bundled) or `sqlx` (sqlite) | WAL, file mode 600 |
| CLI | `clap` (derive) | |
| logging | `tracing`, `tracing-subscriber` | |
| errors | `thiserror` (lib), `anyhow` (bin) | good Rust-learning moment |
| time | `jiff` or `chrono` (+ `chrono-tz`) | digest scheduling in local time |
| email building | `mail-builder` (Stalwart) or `lettre`'s message builder | for the digest APPEND |
| later: Laya | `ort` (ONNX Runtime) + `tokenizers` | see CLASSIFIER.md |

## Module layout (proposal)

```
src/
  main.rs            # clap CLI → commands
  config.rs          # TOML → typed config, validation, password_command
  store.rs           # SQLite: mailbox state, processed log, flag ownership, rules, known senders, digest state, label dataset
  message.rs         # parse raw → ParsedMessage (auth results, bulk headers, body text, spoof signals)
  decider/
    mod.rs           # trait Decider, Evidence, Decision
    llm.rs           # OpenAI-compatible client, prompt, structured output fallback, parsing
  engine.rs          # rules → known sender → decider → FlagChoice (pure, no IO: easy to unit-test)
  flags.rs           # colour ⇄ keyword bits, ownership checks, priority
  imap/
    session.rs       # connect, TLS, capabilities, folder resolution (special-use, delimiter, namespace)
    worker.rs        # per-account loop: IDLE, fetch new UIDs, apply flags, auto-clear pass
  digest/
    build.rs         # stored rows -> digest model (pure); uses the stored verdict reasons, no model call
    render.rs        # model -> text + escaped HTML -> multipart/alternative (mail-builder)
    schedule.rs      # next_step (pure), the daily loop, catch-up after sleep
    deliver.rs       # period, APPEND (or a preview file for a dry-run account), own-digest records
contrib/launchd.plist
tests/               # integration tests against a real IMAP server
```

Keep `engine.rs` and `flags.rs` free of IO. Most logic gets unit-tested there.

## IMAP gotchas (learned in the Python prototype)

- Always `BODY.PEEK[...]`. Plain `BODY[]` marks mail as read.
- Track UIDVALIDITY; on change, reset state.
- First run: start at `UIDNEXT-1`; don't touch history unless asked (`backfill`).
- Re-issue IDLE well before 29 min (the prototype used about 5 min with 15 s checks so stop/wake
  is responsive).
- Folder names: resolve special-use (`\Junk`, `\Trash`, `\Sent`) via LIST flags; respect the
  hierarchy delimiter and personal namespace prefix; fall back to plain names.
- Keywords: check `PERMANENTFLAGS` for `\*` before setting `$MailFlagBit*`.
- Set the flag and colour bits in **one STORE** (`+FLAGS (\Flagged $MailFlagBit0)`) and remove
  stale bits with `-FLAGS` in the same pass.
- No MOVE → COPY + `\Deleted` + `UID EXPUNGE` (only with UIDPLUS; never a plain EXPUNGE). Not
  needed in v1, since there are no moves.
- Outlook.com/M365 have no IMAP password auth (OAuth2 only); Gmail works with app passwords.
- Server-side flag changes made by the user arrive via FLAGS/FETCH responses. For auto-clear,
  re-fetch FLAGS before changing anything.

## Testing

- Port the Python prototype's 34 tests as the behavioural spec (the prototype is kept outside
  this repo; see `CLAUDE.local.md` for its path).
- Integration tests need a real IMAP server. **Decision (task 3.1): Dovecot 2.3 in Docker**
  (`dovecot/dovecot:2.3.21.1`, config in `tests/dovecot/dovecot.conf`). Why Dovecot: Maildir
  accepts new keywords and reports `PERMANENTFLAGS (... \*)`, which the `$MailFlagBit*` colours
  need; special-use folders (`\Sent`) are a config line; IDLE and UIDPLUS are on by default; and
  it is what many self-hosted users actually run. GreenMail (Java) was rejected: it is a test double rather
  than a full server, and its documentation does not promise `\*` in `PERMANENTFLAGS` or
  special-use folders, which are exactly what these tests exist to check. pymap
  (used by the prototype) was rejected because of its asyncio GC bug and because the Rust tests
  should not need Python.
- The tests that need the server are `#[ignore]`d, so plain `cargo test` stays fast and works
  without Docker (this Mac has none). The `imap-integration` CI job on Linux starts the container
  and runs `cargo test --test imap_integration -- --ignored`. To run them by hand, see the
  header of `tests/common/mod.rs`. Every test logs in as a fresh user (the static userdb accepts
  any name), so tests never share mail.
- Stub LLM: a tiny local HTTP server returning canned verdicts keyed on the subject (the prototype
  has one).
- CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on macOS and Linux, plus the
  Linux-only Dovecot job above.

## Security checklist

- Loopback-only for any future web UI, with a Host-header check (DNS rebinding), CSRF tokens on
  POST, escaping of all email-derived text, and a strict CSP (all done in the prototype).
- Any URL from an email (unsubscribe, later) must resolve to a public IP and be IP-pinned (SSRF).
- The prompt-injection blast radius is limited by design: the model returns typed fields only.
- The DB holds metadata only (sender, subject, verdict, reason). It must never hold bodies or
  passwords, except an explicitly opt-in training dataset.
