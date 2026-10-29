# Let's Triage Mail

Local-first email triage for **any IMAP account**, written in Rust. New mail is classified by a
**local model** (Ollama, LM Studio, llama.cpp, MLX), so nothing leaves your machines. Let's Triage
Mail sets **Apple Mail flag colours** on the server and writes a **daily digest email** into your
mailbox.

> **Status: v1 works, not released yet.** It runs every day on its author's own accounts. There
> are no prebuilt binaries or Homebrew formula yet, so you build it from source (below).

## What it does

- Classifies new inbox mail and sets flag colours that mean "action needed": Reply needed,
  Deadline, To pay, Bookkeeping, Unsure. Red stays reserved for you. See
  [docs/FLAGS.md](docs/FLAGS.md).
- Clears its own flags again when you have replied or the date has passed. A flag you set or
  changed is never touched.
- Writes one daily digest per account into the mailbox over IMAP (no SMTP).
- Watches the Spam folder read-only and lists mail from people you know that landed there.
- Never moves, deletes or hides mail. It only sets and clears its own flags.

Flags are stored on the server, so they show in Apple Mail on the Mac and the iPhone, and in any
client that shows flags, with nothing to install in the mail client.

## Quickstart

You need macOS or Linux, Rust 1.85 or newer ([rustup.rs](https://rustup.rs)), a local model
server, and an IMAP account that allows app passwords.

### 1. Install

```sh
git clone https://github.com/lets-code-apps/lets-triage-mail
cd lets-triage-mail
cargo install --path .
```

This puts `lets-triage-mail` in `~/.cargo/bin`.

### 2. Start a model

Any server with an OpenAI-compatible `/v1/chat/completions` endpoint works. With Ollama:

```sh
ollama serve
ollama pull qwen3:14b
```

### 3. Write the config

```sh
mkdir -p ~/.config/lets-triage-mail
cp config.example.toml ~/.config/lets-triage-mail/config.toml
```

Edit `[llm]` to point at your model and add one `[[accounts]]` block per mailbox. Every option is
explained in [config.example.toml](config.example.toml). Passwords never go in the file. On
macOS, store each one in the Keychain once (you are asked to type it):

```sh
security add-generic-password -s lets-triage-mail -a personal -w
```

and read it back with `password_command`, as in the example. Without a Keychain, set
`LETS_TRIAGE_MAIL_PASSWORD_<NAME>` instead. Most providers need an app-specific password.

### 4. Check the setup

```sh
lets-triage-mail check
```

`check` reports `OK`, `WARN` or `FAIL` for each area, with a hint for every problem:

- **Config:** the file is valid, whether dry-run is on, and the state database can be opened.
- **Model:** the server answers, your model exists, and one made-up message is classified (with
  the time it took and the structured-output mode that works).
- **Each account:** password lookup, login over TLS, IDLE/UIDPLUS/MOVE support, inbox size, the
  Sent and Spam folders, and whether the server allows custom keywords. Flag colours are stored
  as keywords, so an account without them can only get the digest.

Use `--account NAME` to check one account and `--json` for machine-readable output. The exit code
is non-zero if anything failed. `check` never changes anything on the mail server.

### 5. Watch in dry-run

```sh
lets-triage-mail run
```

`run` keeps one connection per account, classifies new mail as it arrives and logs the flag it
would set. It also reads each Sent folder every few hours to learn who you write to. With
`[policy] dry_run = true` (the default) nothing is changed on the server; each account's digest
is saved as `~/.local/share/lets-triage-mail/digests/<account>-<date>.eml` instead (open it in
Mail to preview).

After a day or two, look at the verdicts:

```sh
lets-triage-mail stats --days 7
```

Press Ctrl-C (or send SIGTERM) to stop; a second Ctrl-C exits at once. If one account cannot log
in, the others keep running and the failed one is retried after 5 minutes, then 10, up to an
hour.

### 6. Go live on one account

When `stats` shows verdicts you agree with, turn flagging on for **one** account first by adding
`dry_run = false` to that `[[accounts]]` entry and leaving `[policy] dry_run = true`:

```toml
[[accounts]]
name = "zoho"
# ...
dry_run = false   # only this account writes flags and gets the digest; the others stay in dry-run
```

Restart `run`. It then sets a flag colour on new mail there and clears it again when you have
replied (orange "Reply needed") or the date has passed (yellow "Deadline"). It never uses red,
never touches a message you flagged, and leaves a message alone for good once you change its
flag. A mailbox without custom keywords cannot store colours, so `run` refuses to go live on it;
set `flagging = false` there to get only the digest.

`backfill --days 7 --account zoho` runs the same pipeline over the last week of the inbox
(respecting dry-run) without moving the worker's position.

In Mail on the Mac you can rename the flags in the sidebar (expand Flagged), so "Orange" reads
"Reply needed".

### 7. Run it in the background (macOS)

A LaunchAgent starts the service at login and restarts it after a crash. It runs as you, so the
login Keychain works for `password_command`. It assumes the binary is in `~/.cargo/bin`;
otherwise edit the path in the plist.

```sh
L=io.github.lets-code-apps.lets-triage-mail
mkdir -p ~/Library/Logs/lets-triage-mail ~/Library/LaunchAgents
sed "s|/Users/YOU|$HOME|g" contrib/launchd/$L.plist > ~/Library/LaunchAgents/$L.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$L.plist   # install and start
launchctl print gui/$(id -u)/$L                                    # status
launchctl kickstart -k gui/$(id -u)/$L                             # restart (after a config change)
launchctl bootout gui/$(id -u)/$L                                  # stop; delete the plist to uninstall
```

Logs: `~/Library/Logs/lets-triage-mail/lets-triage-mail.log`. A failed start (for example a
config error) is retried every 30 seconds; a clean stop is not. A Linux systemd unit would be a
welcome contribution.

## Try the model first: classify your own mail

You can judge the model on real messages before giving it any mailbox.

1. Export some messages from Apple Mail: select them and drag them to a folder in Finder, for
   example `~/Desktop/mail`. Each one becomes an `.eml` file.
2. With the config from step 3 (only `[llm]` is used), run:

   ```sh
   lets-triage-mail classify ~/Desktop/mail/*.eml
   ```

Each file prints its category, confidence, reason and the flag it would get. Add `--json` for
one JSON object per line. Nothing is sent anywhere except to your own model server, message
bodies are never printed or stored, and no mailbox is touched.

## The daily digest

Once a day at `[digest] time` (local time) `run` appends one summary email per account to
`[digest] folder` (default `INBOX`), unread: what needs you, grouped by flag, mail from people you
know that landed in Spam, and counts of the rest. After a sleep it catches up on wake. English
only. It uses the model's one-line reasons from classification, so it makes no model calls and
stores no bodies. Its only image travels inside the message, so opening it loads nothing from the
network.

```sh
lets-triage-mail digest --dry               # print today's digest, change nothing
lets-triage-mail digest --account zoho      # write it now
```

## Providers

What has been tried so far. Reports for other providers are very welcome: open a
[provider report](https://github.com/lets-code-apps/lets-triage-mail/issues/new?template=provider-report.yml)
with the output of `lets-triage-mail check --account NAME`.

| Provider | Login | Flag colours | Digest | Notes |
|---|---|---|---|---|
| Zoho Mail | password, or app password with 2FA | yes | yes | in daily use |
| Gmail | app password | yes (custom keywords allowed) | yes | `check` passes; flagging not yet run live |
| wp.pl | password | no (no custom keywords) | yes | `flagging = false`; set `provider_headers = "wp"` (no `Authentication-Results`) |
| Dovecot | password | yes | yes | the integration tests run against it |
| iCloud, Fastmail | app password | should work (keywords allowed) | should work | not tried yet |
| Outlook.com, Microsoft 365 | OAuth only | no | no | no IMAP password login; not supported |

## Docs

[docs/SPEC.md](docs/SPEC.md) has the full spec. The design notes are in [docs/](docs/): flags,
classifier, architecture and the decision log.

The design was proved first in an unpublished Python prototype (folder moves, training folders,
unsubscribe queue, review UI). This repository is the Rust rewrite.

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md). Please report security problems privately, as described
in [SECURITY.md](SECURITY.md), which also has the threat model.

## Licence

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.

---

Made by [Let's Code](https://github.com/lets-code-apps).
