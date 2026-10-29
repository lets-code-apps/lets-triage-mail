# Open-source plan

## Before creating the repo

- [x] Pick a name that is free on GitHub, crates.io and Homebrew: **Let's Triage Mail** (`lets-triage-mail`), repo `lets-code-apps/lets-triage-mail`.
- [x] Licence: MIT OR Apache-2.0 dual (Rust convention).

## Day one

- [ ] README that leads with: a GIF/screenshot of flagged mail in Apple Mail plus a digest email;
      "your email never leaves your machine"; "works with Ollama, LM Studio, llama.cpp, MLX";
      "any IMAP provider".
- [ ] Install: `cargo install`, a Homebrew tap, prebuilt macOS (arm64) and Linux binaries via
      GitHub Releases (cargo-dist is worth a look).
- [x] `check` command that diagnoses setup (model reachable, IMAP caps, keyword support, folders).
- [x] CI: fmt, clippy, tests on macOS and Linux.
- [x] CONTRIBUTING.md, issue templates (bug / provider compatibility report), a SECURITY.md with
      the threat model (hostile email, prompt injection, SSRF).
- [x] Provider compatibility table in the README (iCloud, Fastmail, Dovecot, Proton Bridge,
      Gmail app password, …), filled in by contributors. (Started; reports via the provider template.)

## Launch channels

r/selfhosted, r/LocalLLaMA, r/rust (as a "learning Rust, built X" post), Hacker News (Show HN),
Polish dev communities.

## Story angles

- Privacy-first AI email triage that works with **any** IMAP account and **any** mail client.
- Apple Mail flag colours via RFC 9979: works on the iPhone with no app.
- Later: "fine-tune your own tiny triage model (Laya) on your own mail, locally."
