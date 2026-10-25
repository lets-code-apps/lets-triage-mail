# v1 implementation plan

The path from today's code (`flags.rs`, `engine.rs`) to a finished v1: classify and flag new mail,
plus the daily digest (see `SPEC.md`). Each task below is sized for one agent session and one pull
request.

## The order, and why

**The first real test is the riskiest moment:** our code touches Piotr's real mailbox for the
first time. So the plan builds up trust in three milestones, each one riskier than the last:

| Milestone | What runs | Risk to the mailbox |
|---|---|---|
| **A. Offline** (end of phase 1) | `classify FILE.eml` against the local LLM | none: no IMAP at all |
| **B. Read-only** (end of phase 3) | `check`, then `run` in dry-run mode on a real account | read-only: `BODY.PEEK[]`, no STORE |
| **C. Flagging** (end of phase 4) | `run` with `dry_run = false` | sets and clears our own flag keywords only |

The digest (phase 5) and packaging (phase 6) come after the flagging pipeline has proven itself.

**What must exist before milestone B** (the first contact with a real account):
- `dry_run = true` as the config default, and no code path that sends `STORE` while it is on
- every fetch uses `BODY.PEEK[]` (test-enforced)
- the `check` command, so a broken setup fails loudly before `run`
- the model-failure rule: a model error means no action and a retry later, never a guess
- state saved per `(account, mailbox, UIDVALIDITY)`, so a restart never re-processes history

## Rules for every task (agent brief)

Every agent reads `CLAUDE.md` first. In addition:

1. **Hard rules are non-negotiable:** no cloud calls, never set red, only clear our own flags, no
   bodies in the DB, `BODY.PEEK[]` only, email content is hostile input.
2. **Keep the IO-free core IO-free.** `flags.rs`, `engine.rs` and the parsing/rendering logic take
   plain values and return plain values. IMAP, HTTP and SQLite live at the edges.
3. **Tests come with the code.** Port the matching prototype tests (path in `CLAUDE.local.md`)
   as the behavioural spec. Drop tests for things that are not in v1 (moves, unsubscribe, web UI).
4. **Before finishing:** `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings` and
   `cargo test` all pass.
5. **Match the existing style:** doc comments on every public item, comments that explain *why*,
   readable code over clever code (Piotr is learning Rust from this codebase).
6. **Check crate versions and APIs** on crates.io/docs.rs before using them; the candidate list in
   `ARCHITECTURE.md` is a starting point, not a decision.
7. **End the PR description with a "Rust concepts used" section:** a short list of the language
   features and patterns the task introduced (for example "first `async fn`", "`Arc<Mutex<_>>`
   shared between tasks"). Piotr walks through these later with Claude as a guide.
8. **Work on a branch in its own worktree.** Tasks in the same phase may run in parallel; expect
   small merge conflicts in `lib.rs`, `Cargo.toml` and `main.rs` only.

## Phase 0: Foundations (done or nearly)

| # | Task | Status |
|---|---|---|
| 0.1 | `flags.rs`: colour ⇄ IMAP keywords, red never settable | done |
| 0.2 | `engine.rs`: sender screening, verdict → flag | done |
| 0.3 | **App skeleton**: `clap` CLI with all `SPEC.md` §4 subcommands as stubs, `tracing` logging, `anyhow` in the binary and `thiserror` in the library, `tokio` runtime in `main` | to do |

**0.3 details.** `main.rs` parses the CLI and dispatches; each command lives in `src/cli/<name>.rs`
and returns `anyhow::Result<()>`. Unimplemented commands print "not implemented yet" and exit
non-zero. Add `--config PATH` (default `~/.config/lets-triage-mail/config.toml`) and `-v/-vv`.
Tests: `clap`'s `debug_assert` test, and `--help` works.
*Depends on:* nothing. *Learning:* external crates, `derive` macros from libraries, `Result`
and `?`, the difference between library and application errors.

## Phase 1: Offline pipeline → milestone A

Tasks 1.1–1.3 can run in parallel after 0.3.

### 1.1 Config (`config.rs`)
- TOML → typed structs with `serde`: `[llm]`, `[policy]` (dry_run, min_confidence,
  known_senders_from_sent, authserv_id), `[flags.<name>]` (enabled, colour, name),
  `[digest]`, `[[accounts]]`. See `SPEC.md` §3.
- Validation with clear error messages: confidence within 0–1, a flag colour must not be red,
  account names unique, TLS required unless the host is loopback, `max_concurrent ≥ 1`.
- Convert `[flags.*]` into `engine::FlagPolicy` and a colour per `TriageFlag` (colours become
  configurable here; add `colour_for(TriageFlag)` instead of the hard-coded default).
- Passwords: `password_command` (run through `sh -c`, take stdout, trim) or env var
  `LETS_TRIAGE_MAIL_PASSWORD_<ACCOUNT>`. Never log the password; wrap it in a type whose
  `Debug` prints `***`.
- Ship `config.example.toml` at the repo root, adapted from the prototype's (v1 sections only).
- *Tests:* the example config parses; each validation error; the password type never prints.
- *Learning:* `serde` derive, `#[serde(default)]`, newtypes, implementing `Debug` by hand,
  `std::process::Command`.

### 1.2 Message parsing (`message.rs`)
- Raw RFC 822 bytes → `ParsedMessage`: from address and display name, reply-to, to/cc, subject
  (decoded), date, Message-ID, In-Reply-To/References, list headers (List-Id, List-Unsubscribe,
  Precedence, Auto-Submitted), plain-text body (HTML flattened, scripts and styles dropped),
  link count, attachment names, size.
- Authentication: SPF/DKIM/DMARC from the **topmost** `Authentication-Results`, or only the one
  whose authserv-id matches config. Several DKIM signatures: one pass is enough.
- Spoof signals: Reply-To domain differs from From, display name contains another address.
- A headers-only mode for messages over 2 MB.
- Crates: `mail-parser`; an HTML-to-text crate (check which handles garbage input without
  panicking).
- *Tests:* port `test_message.py` (minus the one-click unsubscribe tests), using `.eml` fixtures in
  `tests/fixtures/`.
- *Learning:* lifetimes in practice (borrowing from the parsed buffer vs owning `String`s),
  `Option` chaining, iterators over headers.

### 1.3 Model decider (`decider/mod.rs`, `decider/llm.rs`)
- `trait Decider { async fn decide(&self, evidence: &Evidence) -> Result<Verdict, DecideError>; }`
  so a Laya backend can be added later (see `CLASSIFIER.md`).
- `Evidence` is built from `ParsedMessage` plus locally computed signals ("user has written to this
  domain", auth results). It holds only what the prompt needs.
- `LlmDecider`: OpenAI-compatible `/v1/chat/completions` over `reqwest` (rustls). Prompt per
  `CLASSIFIER.md`: categories defined, email marked untrusted, body fenced between `<<<BODY` and
  `BODY>>>` (markers stripped from the body), truncated to `max_body_chars`.
- Output: one call returns `category`, `needs_reply`, `deadline`, `payment`, `confidence`,
  `reason`. Try `json_schema`, fall back to `json_object`, then plain text; remember which mode
  works. Strip `<think>` and code fences, clamp confidence, reject unknown categories.
- Guard rail: "spam" on fully authenticated bulk mail becomes "marketing".
- A process-wide semaphore (`max_concurrent`, default 1) so the voice assistant's LLM is not starved.
- Any failure (timeout, HTTP error, unparseable output) → `Err`, never a default verdict.
- *Tests:* port `test_classifier_engine.py` (the parsing and prompt tests); a fake LLM HTTP server
  in tests (a small local server, or `wiremock`).
- *Learning:* **the first trait we define ourselves**, `async fn`, `tokio`, `serde_json`, error
  enums with `thiserror`, `Arc` and `Semaphore`.

### 1.4 `classify FILE.eml` command (milestone A)
- Load config, parse the file, build evidence, call the decider, run `engine::choose_flag` (with
  `Trust::Unknown`, since there is no DB yet), and print verdict, reason and chosen flag/colour.
- `--json` for machine-readable output.
- *Depends on:* 1.1, 1.2, 1.3.
- **Milestone A check:** Piotr exports 20–30 real messages from Apple Mail (drag to Finder makes
  `.eml` files), runs `classify` on each against his local model, and judges the verdicts. Tune the
  prompt here, before any IMAP work.

### 1.5 Tuning after the first real data (2026-10-25)
Milestone A ran on 16 real messages. Most verdicts were right. Three gaps showed up:
- **Rotating-address spam.** Casino spam arrives 5–10 times a day, each from a new random
  address, so a block rule cannot catch it. It was labelled "marketing". It has signals our
  code can check without the model:
  - **Invalid sender address:** the domain has two dots in a row (`a@x.cz..sn.live.com`).
  - **The recipient's own name inside a stranger's From:** the local part starts with the
    recipient's own local part plus random characters
    (`me.name.2lkxp6bb.fnvrb@other.example`), or the display name equals the recipient's local
    part or address.
- **Provider headers.** Some providers (wp.pl) do not write `Authentication-Results`; they write
  their own headers instead (`X-WP-DKIM-Status`, `X-WP-SPAM`). Without them, SPF/DKIM is
  "unknown" for every message on that account.
- **Prompt.** A delivery date was read as a deadline. A marketing agency sending for a brand
  (brand ≠ sending domain, valid DKIM) was called phishing.

Work:
- A deterministic **pre-check** before the model: certain spam gets a verdict from code (source
  `rule`), so the model is not called. This saves about 12 s per message on a thinking model.
  Shared by `classify` and the worker.
- Opt-in support for provider authentication and spam headers, set in the config. It is opt-in
  because a sender can forge headers; only trust them when the user's provider stamps them.
- Prompt: a deadline is an action the user must take by a date. Phishing needs a credential or
  payment lure, not only a brand–domain mismatch.
- *Depends on:* 1.2, 1.3, 1.4, 3.5.

## Phase 2: Storage

### 2.1 SQLite store (`store.rs`)
- `rusqlite` (bundled) with WAL, file mode 600, path from config (default
  `~/.local/share/lets-triage-mail/state.db`). Schema migrations by `user_version`.
- Tables: `mailbox_state` (account, mailbox, uidvalidity, last_uid), `processed` (one row per
  message: ids, sender, subject, category, verdict fields, flag, reason, timestamp; **no body**),
  `flag_ownership` (account, mailbox, uidvalidity, uid, message_id, colour, reason, set_at,
  `user_owned`), `rules` (pattern, allow/block), `known_senders` (account, address, domain),
  `retry_queue`, `digest_state` (account, last_digest_date), `labels` (evidence features + verdict +
  later user correction; the future Laya dataset).
- A `Store` API with methods named after what callers need (`match_rule`, `is_known_sender`,
  `record_flag`, `mark_user_owned`, …). Calls from async code go through
  `tokio::task::spawn_blocking`.
- *Tests:* in-memory DB per test; port the store-related parts of the prototype tests.
- *Learning:* SQL from Rust, mapping rows to structs, `spawn_blocking` (sync code inside async).

### 2.2 `rules` and `stats` commands
- `rules list|allow|block|remove PATTERN` (address or `@domain`, normalised to lowercase).
- `stats [--days N]`: counts per category and per flag, top senders, model failures.
- *Depends on:* 2.1.

Phase 2 can start as soon as phase 0 is done, in parallel with phase 1.

## Phase 3: IMAP, read-only → milestone B

### 3.1 Test harness for IMAP
- A real IMAP server for integration tests: **GreenMail or Dovecot in Docker** (check which
  supports custom keywords, `PERMANENTFLAGS \*`, special-use and IDLE). A helper that seeds
  messages with APPEND. Reuse the fake LLM server from 1.3.
- CI: run integration tests on Linux, where Docker is available; mark them `#[ignore]` locally
  unless an env var says the server is up.
- *Can start in parallel with phase 1.*

### 3.2 IMAP session (`imap/session.rs`)
- Connect with TLS (or STARTTLS), log in, read capabilities (IDLE, UIDPLUS, MOVE, keywords).
- `PERMANENTFLAGS` contains `\*` → keywords allowed; otherwise flagging is impossible, and `check`
  says so.
- Resolve folders: special-use (`\Sent`) via LIST, hierarchy delimiter, personal namespace,
  fallback names.
- **Only `BODY.PEEK[]` fetches exist in the code.** Add a test that greps the source for `BODY[`
  without `PEEK`.
- Crate: `async-imap` (check maintenance and IDLE/keyword support first; fallback: the sync
  `imap` crate on blocking threads).
- *Learning:* async IO, TLS setup, streams.

### 3.3 `check` command
- Per account: connect, show capabilities, keywords allowed, resolved Inbox and Sent, message
  counts. Model: reachable, model present, one tiny test classification, structured-output mode.
  Config: validates. Exit code non-zero on any failure, with an actionable message per problem.
- *Depends on:* 1.1, 1.3, 3.2.

### 3.4 Known senders from Sent
- Harvest To/Cc/Bcc addresses from the Sent folder via ENVELOPE (no bodies), incrementally by UID.
  Feed `Store::is_known_sender`.
- *Depends on:* 2.1, 3.2.

### 3.5 Account worker, dry-run (`imap/worker.rs`)
- One task per account. First run: store `UIDNEXT-1` and leave history alone. Track
  UIDVALIDITY; reset on change.
- New mail: IDLE (re-issued every ~5 min, well under 29) or 60 s polling without IDLE. Fetch
  `RFC822.SIZE` and `FLAGS` first, then `BODY.PEEK[]` if ≤ 2 MB, else headers only.
- Skip: already flagged by the user, own address. Then `screen_sender` → decider →
  `choose_flag` → record in `processed` and `labels`. **In dry-run, stop here and log what would
  have been set.**
- Model failure → retry queue with backoff. Connection loss → reconnect with exponential backoff.
- *Tests:* port `test_integration.py` (live processing, dry-run touches nothing, IDLE wake-up),
  minus moves and training folders.
- *Depends on:* 1.2, 1.3, 2.1, 3.2.

### 3.6 `run` command (dry-run only at this stage)
- Start one worker per account plus the Sent harvester, share the decider and store, handle
  Ctrl-C / SIGTERM with a clean shutdown.
- **Milestone B check:** `check` passes on Piotr's accounts, then `run` in dry-run for a few days.
  Review with `stats` and the logs: are the would-be flags right? Is the IMAP state stable across
  sleep, wake and network changes?
- *Learning:* spawning tasks, `JoinSet`, shared state with `Arc`, cancellation.

## Phase 4: Flagging → milestone C

### 4.1 Apply flags
- When `dry_run = false`: one `UID STORE +FLAGS (\Flagged $MailFlagBit…)` plus `-FLAGS` for stale
  bits, from `FlagColour::store_change`. Record ownership in the same step; if the STORE fails,
  record nothing.
- Never touch a message the user already flagged (re-check FLAGS right before the STORE).

### 4.2 Auto-clear pass
- Periodically, for flags we own: re-fetch FLAGS first. If they differ from what we set, mark the
  message user-owned and never touch it again.
- Clear "Reply needed" when a reply appears in Sent (match `In-Reply-To`/`References` with the
  harvester from 3.4), and "Deadline" when the date has passed (add `jiff` here and turn
  `Verdict::deadline` into a real date).
- Clearing removes `\Flagged` and our bits in one STORE.
- *Tests:* user changed the colour → user-owned and untouched; user unflagged → untouched forever;
  reply in Sent → cleared.

### 4.3 `backfill [--days N] [--account X]`
- Process older inbox mail through the same pipeline, oldest first, respecting dry-run and
  the model semaphore.

**Milestone C check:** turn off dry-run on one account; watch Apple Mail on Mac and iPhone.

Tasks 4.1–4.3 depend on phase 3. 4.2 and 4.3 can run in parallel after 4.1.

## Phase 5: Daily digest

### 5.1 Build and render (`digest/build.rs`)
- Gather the last 24 h from the store: flagged items by colour, newsletters, noise counts, unsure
  items. Ask the model for one-line summaries (newsletters, flagged items) through the same
  decider client and semaphore.
- Render plain text and simple HTML (light and dark mode, no remote content), as
  `multipart/alternative`, with our `Message-ID` domain and the `X-Lets-Triage-Mail: digest`
  header. Crate: `mail-builder`. Summaries are model output from hostile input: escape them in HTML.
- *Tests:* snapshot tests of the rendered text and HTML from a fixed store.

### 5.2 Deliver and schedule (`digest/schedule.rs`)
- APPEND without `\Seen` to the configured folder, once a day at the configured local time.
  Catch-up after sleep; `last_digest_date` guarantees one digest per day per account.
- The worker skips our own digest when classifying.
- `digest [--account X] [--dry]`: build and print, or append immediately (the scheduled digest
  runs inside `run`).

## Phase 6: Ship v1

| # | Task |
|---|---|
| 6.1 | `contrib/` launchd plist with KeepAlive; install notes in the README |
| 6.2 | README quickstart: install, config, `check`, dry-run, going live; provider compatibility table |
| 6.3 | Releases: `cargo-dist` for macOS arm64 and Linux binaries, Homebrew tap `lets-code-apps/tap` |
| 6.4 | CONTRIBUTING.md, issue templates, SECURITY.md with the threat model (`OPEN_SOURCE.md`) |
| 6.5 | Final review against every hard rule in `CLAUDE.md`; a security review of prompt handling and HTML escaping |

## Dependency map

```
0.3 ─┬─ 1.1 ─┐
     ├─ 1.2 ─┼─ 1.4 (A)
     ├─ 1.3 ─┘
     ├─ 2.1 ── 2.2
     └─ 3.1
1.1 + 1.3 + 3.2 ── 3.3
2.1 + 3.2 ── 3.4
1.2 + 1.3 + 2.1 + 3.2 ── 3.5 ── 3.6 (B) ── 4.1 ─┬─ 4.2 ── (C)
                                                 └─ 4.3
3.6 ── 5.1 ── 5.2
everything ── 6.x
```

**Parallel waves:** wave 1: 0.3. Wave 2: 1.1, 1.2, 1.3, 2.1, 3.1, 3.2. Wave 3: 1.4, 2.2, 3.3,
3.4. Wave 4: 3.5 → 3.6. Then phases 4, 5 and 6 in order.

## Open questions (decide before the task that needs them)

- **Digest language** (before 5.1): English only, or follow `[digest] language`?
- **Known senders** (before 3.4): every address ever written to, or only in the last N years?
- **IMAP test server** (in 3.1): GreenMail or Dovecot, depending on keyword and IDLE support.
- **`async-imap` vs `imap`** (in 3.2): pick after checking maintenance and features.
