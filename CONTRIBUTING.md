# Contributing

Thanks for helping. Bug reports, provider reports and small focused pull requests are all
welcome.

## Reports

- **Bug:** use the bug template. Include `lets-triage-mail --version`, your OS, and the log lines
  around the problem (`-v` for more). Remove addresses and subjects you don't want public.
- **Provider report:** if you ran `check` against a provider that is not in the README's table,
  or it behaves differently, use the provider template. Paste the `check --account NAME` output;
  it contains no passwords, but it does contain your server and folder names.
- **Security problems:** not in public issues. See [SECURITY.md](SECURITY.md).

Never attach real email (`.eml` files) to an issue. If a message triggers a bug, describe its
headers and structure, or make a minimal fake one.

## Building and testing

```sh
cargo build
cargo test                                   # unit tests, no network
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs the same on macOS and Linux. The IMAP integration tests run against a real Dovecot in
Docker and are ignored by plain `cargo test`:

```sh
docker run -d --name ltm-dovecot -p 1143:143 \
  -v "$PWD/tests/dovecot/dovecot.conf:/etc/dovecot/dovecot.conf:ro" \
  dovecot/dovecot:2.3.21.1
cargo test --test imap_integration -- --ignored
```

On Apple silicon the amd64 test image needs QEMU emulation; it crashes under Rosetta (Docker
Desktop: turn off "Use Rosetta for x86/amd64 emulation").

## Pull requests

- Open an issue first for anything bigger than a fix, so we can agree on the approach.
- One change per pull request, with tests for new behaviour. Keep `fmt` and `clippy` clean.
- Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the module layout and
  [docs/DECISIONS.md](docs/DECISIONS.md) before changing a decision; a new decision gets a row
  there with its reason.
- Docs, comments and commit messages are in English.
- Contributions are dual licensed MIT OR Apache-2.0, as described in the README.

## Rules every change keeps

These are the project's promises to its users. A change that breaks one will not be merged.

- **No cloud AI.** Email is only ever sent to the model endpoint the user configured.
- **No message bodies stored.** The database holds metadata only.
- **v1 never moves, deletes or hides mail.** It only sets and clears its own flag keywords.
- **Red is the user's.** The service never sets a red flag.
- **Only clear our own flags,** and never touch a message once the user changed its flag.
- **Email content is hostile input.** The model returns typed fields only; nothing in an email
  can trigger an action. Bodies are fenced as untrusted in prompts.
- **Nothing gets marked as read:** fetch with `BODY.PEEK[]`.
