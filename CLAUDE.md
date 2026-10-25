# CLAUDE.md: project memory

Read this first. Details live in `docs/`.

## What this is

An open-source, **local-first email triage service written in Rust**. It connects to any IMAP
account, classifies new mail with a **local model** (nothing leaves the user's machines), sets
**Apple-Mail-compatible flag colours** on the server, and writes a **daily digest email** into
the mailbox.

- Name: **Let's Triage Mail**; repo, crate and binary `lets-triage-mail`; GitHub
  `lets-code-apps/lets-triage-mail`. It follows Piotr's "Let's ..." brand pattern:
  `lets-<verb>-<object>` for services, `lets-<verb>` for tools people use directly.
- Owner: Piotr, a senior developer with about 10 years of experience (Java, PHP, Flutter, Swift;
  earlier Python). He is **learning Rust with this project**: explain Rust concepts (ownership,
  lifetimes, async and tokio, error handling, traits) as they come up, and prefer idiomatic,
  readable code over clever code.
- Goal: a public GitHub repo that earns stars and is useful to people on the same stack (Mac,
  Apple Mail, local LLM through Ollama, LM Studio or MLX, self-hosted or non-Google IMAP).
- All docs, code comments and commit messages are in **English**.

## v1 scope (agreed 2026-10-25)

1. **Classify and flag.** Classify new inbox mail and set flag colours (see `docs/FLAGS.md`).
   **v1 does not move mail and does not unsubscribe.** Those exist in the Python prototype and are
   planned for later versions.
2. **Daily digest.** Once a day, write one summary email per account into the mailbox with IMAP
   APPEND (no SMTP). See `docs/SPEC.md`.

**Noted:** Piotr is strongly considering the small **Laya** decision model (an open alternative to
JEV) for category decisions instead of a general LLM. Plan: v1 uses a local LLM behind a swappable
`Decider` trait and logs labelled verdicts; Laya becomes the fast path once there is data to
fine-tune and calibrate it. See `docs/CLASSIFIER.md`.

Out of scope for v1: folder moves, unsubscribe queue, web review UI, MailKit (Swift) extension,
"summarize selected message" shortcut, OAuth providers (Outlook.com/M365).

## Hard rules

- **Privacy:** no cloud AI calls. The model endpoint is local (or on the user's LAN). Never store
  message bodies; store metadata only.
- **Never destroy or hide mail in v1.** It only sets or clears flag keywords.
- **Red flag is reserved for the user.** The service never sets red.
- **Only clear flags the service set itself.** Track ownership in the DB; if the user changed a
  flag, leave it alone forever.
- **Email content is hostile input.** The model only returns typed fields; it never triggers
  actions. Fence the body as untrusted in prompts.
- Fetch with `BODY.PEEK[]` so nothing gets marked as read.

## Key docs

- `docs/SPEC.md`: v1 functional spec (flagging, digest, config, CLI)
- `docs/FLAGS.md`: flag colour scheme, IMAP keyword encoding, ownership rules
- `docs/CLASSIFIER.md`: model strategy (local LLM first, Laya later) and prompt design
- `docs/ARCHITECTURE.md`: Rust crate plan, module layout, IMAP gotchas, testing
- `docs/DECISIONS.md`: decision log with reasons, and options we rejected
- `docs/RESEARCH.md`: what we learned about Inbox Zero, Mac mail clients and the Apple Mail extension APIs
- `docs/OPEN_SOURCE.md`: repo, licence and launch checklist
- Python prototype (working name MailGuard, 34 passing tests): kept **outside** this repo so it
  never enters the public history; its local path is in `CLAUDE.local.md`. Treat it as the
  behavioural reference and port its tests as the spec.

## Environment

- Runs on Piotr's Mac Studio (M5 Ultra), which also hosts his local LLM for a Home Assistant
  voice assistant. Keep model concurrency at 1 by default so the two don't compete.
- Accounts: several IMAP accounts that are **not** Gmail/Outlook; Apple Mail is his client.
