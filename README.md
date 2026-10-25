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

## Try it: classify your own mail

You can judge the model on real messages before anything touches a mailbox.

1. Start a local model server (for example `ollama serve` with `ollama pull qwen3:14b`).
2. Export some messages from Apple Mail: select them and drag them to a folder in Finder, for
   example `~/Desktop/mail`. Each one becomes an `.eml` file.
3. Copy the example config and adjust `[llm]` to your model:
   `mkdir -p ~/.config/lets-triage-mail && cp config.example.toml ~/.config/lets-triage-mail/config.toml`
   (or pass `--config PATH`). The `[[accounts]]` block is not used by `classify`.
4. Run it:

   ```sh
   cargo run --release -- classify ~/Desktop/mail/*.eml
   ```

Each file prints its category, confidence, reason and the flag it would get. Add `--json` for
one JSON object per line. Nothing is sent anywhere except to your own model server, message
bodies are never printed or stored, and no mailbox is touched.

## Check your setup

Before pointing the service at a real mailbox, let it test everything it depends on:

```sh
cargo run --release -- check
```

`check` reports `OK`, `WARN` or `FAIL` for each area, with a hint for every problem:

- **Config:** the file is valid, whether dry-run is on, and the state database can be opened.
- **Model:** the server answers, your model exists, and one made-up message is classified (with
  the time it took and the structured-output mode that works).
- **Each account:** password lookup, login over TLS, IDLE/UIDPLUS/MOVE support, inbox size, and
  the Sent folder. A mailbox that does not allow custom keywords fails, because flag colours are
  stored as keywords.

Use `--account NAME` to check one account and `--json` for machine-readable output. The exit code
is non-zero if anything failed. `check` never changes anything on the mail server.

## Run it in dry-run

Once `check` passes, watch your inboxes without changing anything:

```sh
cargo run --release -- run
```

`run` keeps one connection per account, classifies new mail as it arrives and logs the flag it
would set. It also reads each Sent folder every few hours to learn who you write to. Nothing is
changed on the server: `policy.dry_run = false` is refused until flagging exists. Press Ctrl-C
(or send SIGTERM) to stop; a second Ctrl-C exits at once. If one account cannot log in, the
others keep running and the failed one is retried after 5 minutes, then 10, up to an hour.

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
