# Let's Triage Mail

Local-first email triage for **any IMAP account**, written in Rust. New mail is classified by a
**local model** (Ollama, LM Studio, llama.cpp, MLX), so nothing leaves your machines. Let's Triage
Mail sets **Apple Mail flag colours** on the server and writes a **daily digest email** into your
mailbox.

> **Status: early development.** The Rust rewrite has not started yet. Nothing here is usable.

## Planned v1

- Classify new inbox mail and set flag colours that mean "action needed" (Reply needed, Deadline,
  To pay, Bookkeeping, Unsure). Red stays reserved for you. See [docs/FLAGS.md](docs/FLAGS.md).
- One daily digest per account, appended over IMAP (no SMTP).
- v1 never moves, deletes or hides mail. It only sets and clears its own flags.

See [docs/SPEC.md](docs/SPEC.md) for the full spec and [docs/](docs/) for the design notes.

## Background

The design was proved first in an unpublished Python prototype (folder moves, training folders,
unsubscribe queue, review UI). This repository is the Rust rewrite.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.

---

Made by [Let's Code](https://github.com/lets-code-apps).
