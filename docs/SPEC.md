# v1 functional spec

## 1. Watch and classify

- One worker per configured IMAP account. Use IMAP IDLE (re-issue before 29 min; poll every 60 s
  if the server lacks IDLE). Reconnect with exponential backoff.
- On first run, record `UIDNEXT-1` as the starting point and **do not** process existing mail.
  Older mail is processed only by an explicit `backfill --days N` command.
- Track `(account, mailbox, UIDVALIDITY, last_uid)`. If UIDVALIDITY changes, reset the starting point.
- Fetch `RFC822.SIZE` and `FLAGS` first. Fetch the full body with `BODY.PEEK[]` when it is ≤ 2 MB,
  otherwise fetch headers only.
- Skip: messages the user already flagged, and messages from the account's own address.
- Decision pipeline (cheap first):
  1. allow/block rules (address or `@domain`)
  2. **known sender**: anyone the user has written to (harvested from Sent, To/Cc/Bcc via ENVELOPE).
     Trusted unless SPF/DKIM/DMARC fails, which catches spoofing.
  3. model → category + `needs_reply` + `deadline` + `payment` + confidence
- Then map to a flag colour per `FLAGS.md`, set the flag, and record it.
- **dry_run (default on):** classify and log, set nothing. The user flips it off after reviewing.
- If the model is down or returns garbage, **do nothing** to the message, log it, and retry later
  (keep a retry queue; don't silently skip).

Categories (same as the prototype): `personal, transactional, newsletter, marketing,
cold_outreach, spam, phishing`. In v1 the category feeds the digest and the flag logic; nothing
gets moved.

## 2. Daily digest

- Once a day at a configured local time (e.g. 07:30), per account, the service **APPENDs** one email
  to a configured folder (default `INBOX`, so it gets noticed; `LetsTriageMail/Digest` is an option).
  There is no SMTP, so there are no deliverability issues.
- Catch-up: if the Mac was asleep at digest time, send it on the next wake. Store
  `last_digest_date` per account so a day is never duplicated.
- Message: `From: Let's Triage Mail <lets-triage-mail@localhost>` (configurable), `To:` the account address,
  a proper `Date`, a unique `Message-ID`, and `multipart/alternative` (plain text plus simple HTML
  that works in light and dark mode). Appended **without** `\Seen` so it notifies.
- Content, roughly:
  1. **Needs you**: everything flagged in the last 24 h, grouped by colour, one line each
     (sender, subject, one-sentence model summary, deadline/amount if known)
  2. **Newsletters**: a few bullets summarizing what arrived (generative model)
  3. **Noise stats**: counts per category ("41 marketing, 12 cold outreach, 3 spam"), top noisy senders
  4. **Unsure**: items the model was not confident about
- The summaries come from the local generative model. Never include raw bodies beyond short
  excerpts, and never include remote images (digest HTML must not load anything external).
- The digest itself is skipped by the classifier (match on our own Message-ID domain plus a header,
  e.g. `X-Lets-Triage-Mail: digest`).

## 3. Config (TOML, `~/.config/lets-triage-mail/config.toml`)

Sections: `[llm]` (base_url, model, timeout, max_body_chars, max_concurrent=1), `[policy]`
(dry_run, min_confidence, known_senders_from_sent, authserv_id), `[flags.*]` (enabled, colour,
name), `[digest]` (time, folder, from, language), `[[accounts]]` (name, host, port, tls, username,
password_command, inbox, sent_folder).

- Passwords: `password_command` (macOS: `security find-generic-password -s lets-triage-mail -a <acct> -w`)
  or env var `<NAME>_PASSWORD_<ACCOUNT>`. Never store passwords in the config or the DB.
- TLS or STARTTLS required. Plain text only for loopback (tests).
- Most non-Google providers need an **app-specific password** (iCloud certainly does).

## 4. CLI

```
lets-triage-mail run                       # daemon: watch accounts, flag, send digests
lets-triage-mail check                     # test model + each account; show caps (IDLE, MOVE, UIDPLUS, keywords allowed), folders
lets-triage-mail classify FILE.eml         # dry classification of one message, prints verdict + chosen flag
lets-triage-mail backfill [--days 7] [--account X]
lets-triage-mail digest --now [--account X] [--dry]   # build/print or append a digest immediately
lets-triage-mail stats [--days 7]
lets-triage-mail rules list|allow|block|remove PATTERN
```

Packaging: a single static binary, `cargo install`, a Homebrew tap, and a launchd plist
(`contrib/`) with KeepAlive.

## 5. Later (not v1)

The product goal is wider than v1: the service should handle the whole inbox, not only flag it.
Planned after v1, roughly in this order:

1. **Spam and security filtering.** Move spam and phishing to Junk/Trash. This ends the v1 "never
   move or hide mail" rule, so it needs: special-use folder resolution, MOVE (or COPY + `\Deleted`
   + `UID EXPUNGE` with UIDPLUS), a high confidence threshold, a log of every move, a `restore`
   command, and training folders (`NotSpam`/`BlackHole`) as in the prototype. Never delete
   permanently.
2. **Unsubscribing.** The prototype's RFC 8058 one-click unsubscribe queue: the model proposes,
   the user approves in a localhost review UI, then the service sends the one-click POST.
   Unsubscribe URLs come from email, so they are hostile: resolve to a public IP and pin it (SSRF).
3. **Drafted replies.** For mail flagged "Reply needed", draft a suggested reply with the local
   model and APPEND it to the Drafts folder with `\Draft` (threaded via `In-Reply-To` and
   `References`). The user reviews and sends it from their own client. **The service never sends
   mail.** Drafts are model output built from hostile input: plain text only, no links or
   addresses the user did not write.
4. **Home-assistant integration.** Expose triage results (bills and payment due dates, deadlines,
   reminders such as a car lease payment) to Piotr's home assistant and RAG "brain" over a local
   API, so the assistant can surface them. It is one microservice in a family of local `lets-*`
   services. The API shape is not decided yet.
5. **Folder moves** for non-spam categories (Promotions/Newsletters/Cold), as in the prototype.

Other ideas: "Summarize selected message" for Apple Mail via AppleScript + Shortcuts calling
`lets-triage-mail summarize`; an optional Swift MailKit extension for background colours; OAuth2
for Outlook.com/M365 and Gmail.
