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
  `[policy] dry_run` is the default for every account; an account's own `dry_run` overrides it,
  so one low-volume account can go live while the others stay in dry-run. An account with
  `flagging = false` is never written to, whatever `dry_run` says. A live account whose mailbox
  lacks custom keywords (PERMANENTFLAGS without `\*`) refuses to start: a bare `\Flagged` shows
  red in Apple Mail and red is the user's.
- **Setting a flag:** the flags are re-read right before the `STORE`; a message that is already
  flagged is left alone. Ownership is recorded only after the `STORE` succeeded.
- **Auto-clear:** about every 15 minutes the worker looks at the flags it set. If one no longer
  matches what was set, the user changed it and the message is never touched again. "Reply
  needed" clears when the reply is in Sent (`In-Reply-To`/`References`), "Deadline" the day after
  its date; nothing else clears itself.
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
  a proper `Date`, a unique `Message-ID`, and `multipart/alternative`: plain text, plus HTML in the
  let's code email design (`design/email/triage-mail/` in the design repo) for light and dark
  mode, with its header image carried inside the message (`multipart/related`, `cid:`). Appended **without** `\Seen` so it notifies. Subject:
  `Daily triage — <account> — <YYYY-MM-DD>`.
- Period: from the previous digest to now (the last 24 h for the first one), never more than
  48 h, so a week away does not produce a wall of text.
- Content:
  1. **Needs you**: everything flagged (or, in dry-run, would-be flagged) in the period, grouped
     by flag in priority order (Reply needed, Deadline, To pay, Bookkeeping, Unsure), one entry
     each: sender, subject, the model's one-sentence reason, the time, and the date for a
     Deadline. At most 25 per flag; the rest is a count.
  2. **The rest**: counts of newsletters, marketing and cold outreach with the noisiest senders
     (two or more messages), counts of spam and phishing, and one count of other mail.
  An empty day still produces a short digest ("Nothing needed your attention"), so a missing
  digest means the service is down.
- **English only in v1:** `[digest] language` must be `en` (or `en-GB` and the like); any other
  value is a config error, not a silent fallback.
- **No model calls.** Message bodies are never stored, so a fresh summary would need the mail
  again. Each line uses the `reason` the model wrote while classifying. Never include raw
  bodies, and never include remote images (digest HTML loads nothing external). Everything
  that came from email is escaped in the HTML and flattened to one line in the text.
- **Accounts:** the digest follows the account's mode. An account in dry-run (its own `dry_run`,
  else `[policy] dry_run`) is **never appended to**: its digest is saved as
  `<data dir>/digests/<account>-<date>.eml` (next to `state.db`, 0600, the newest 14 kept) and
  logged, so you can preview it, and it says which flags *would* have been set. Every other
  account gets the APPEND, including one with `flagging = false` (there the digest is the main
  output: its would-be flags are derived from the stored verdicts).
- **The digest is never classified.** The worker skips exactly the messages it appended: the
  server's `APPENDUID` (UIDPLUS) is recorded, and the random `Message-ID` as a fallback. The
  `X-Lets-Triage-Mail: digest` header is only a label for people and filters, because a sender
  can write any header and use it to slip past the classifier.

## 3. Config (TOML, `~/.config/lets-triage-mail/config.toml`)

Sections: `[llm]` (base_url, model, timeout, max_body_chars, max_concurrent=1), `[policy]`
(dry_run, min_confidence, known_senders_from_sent, authserv_id), `[flags.*]` (enabled, colour,
name), `[digest]` (time, folder, from, language), `[[accounts]]` (name, host, port, tls, username,
password_command, inbox, sent_folder, junk_folder, watch_junk, flagging, dry_run, provider_headers).

- Passwords: `password_command` (macOS: `security find-generic-password -s lets-triage-mail -a <acct> -w`)
  or env var `<NAME>_PASSWORD_<ACCOUNT>`. Never store passwords in the config or the DB.
- TLS or STARTTLS required. Plain text only for loopback (tests).
- Most non-Google providers need an **app-specific password** (iCloud certainly does).

## 4. CLI

```
lets-triage-mail run                       # daemon: watch accounts, flag, send digests
lets-triage-mail check                     # test model + each account; show caps (IDLE, MOVE, UIDPLUS, keywords allowed), folders
lets-triage-mail classify FILE.eml         # dry classification of one message, prints verdict + chosen flag
lets-triage-mail backfill [--days 7] [--account X]   # same pipeline over the last N days, oldest first; never moves the worker's position
lets-triage-mail digest [--account X] [--dry]         # --dry: print the text, change nothing; else deliver now (append, or a file for a dry-run account)
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

Items 1 and 5 are reshaped by the proposal below: moving mail becomes one action that a user
can choose per folder and category, instead of fixed behaviour.

### Proposal (2026-10-28): folders, modes and actions

**Principle.** The service offers capabilities and recommends a setup; the user chooses how to
use it. Defaults stay conservative; nothing beyond them is forced or forbidden. See
`DECISIONS.md` (2026-10-28).

**One model for everything: folder → mode → actions.**

- **Folder discovery.** The user enters only the connection details (host, port, username,
  password). The service finds the folders itself: it lists every folder with its special-use
  role (`\Inbox`, `\Sent`, `\Junk`, `\Trash`, ...) and stores the list per account (name,
  delimiter, role; no mail). It refreshes the list on each connect, so new and renamed folders
  show up, and `check` prints it. The user sets a mode per folder; a folder without one gets the
  recommended default for its role, otherwise `off`.
- **Modes.**
  - `off`: never opened.
  - `report`: read-only (`EXAMINE`, headers with `PEEK`), no model. The v1 Spam watch is this
    mode with the "trusted sender, DKIM pass" rule.
  - `classify`: the model judges each new message; results go to the digest and `stats`, nothing
    is written to the server.
  - `act`: classify, then carry out the actions configured for the folder.
- **Actions** (for `act`): `flag` (today's flag colours), `move` (to a named folder), `digest`
  (list it). Later actions such as unsubscribe or a drafted reply plug in here as new action
  types, not new code paths.
- **Recommended defaults.** INBOX in `act` with `flag` and `digest`; the Junk folder in `report`;
  Sent read for known senders as today; everything else `off`.
- **Model cost is the user's choice.** A folder in `classify` or `act` costs one model call per
  new message; with a small decision model (Laya) that is cheap even for busy folders. Model
  concurrency stays 1 by default.

**Moving mail, with its safety kit.** A wrong flag costs a glance; a wrong move hides mail. So
`move`:

1. is opt-in per folder **and** per category (for example "newsletters → Newsletters"), with its
   own confidence threshold;
2. never moves mail from a trusted sender (allow rule or known sender), unless the user adds a
   rule saying so;
3. moves only, never deletes: `MOVE`, or `COPY` + `\Deleted` + `UID EXPUNGE` of that UID with
   UIDPLUS; no plain `EXPUNGE`;
4. logs every move (account, from, to, Message-ID, reason, time) and offers `undo` / `restore`;
5. can run in dry-run first, which `check` and the docs recommend. **Dry-run is the user's
   option, not a requirement:** a user who turns a move on explicitly gets it live at once.

**Per-account flags and categories.** Flag profiles are agreed for v1.1; categories are parked
(see below). Today `[flags.*]` is global and the model chooses
from 7 fixed categories; the flag is derived from the verdict by code, and the model never sees
flag names. Different accounts often mean different jobs (home: bills, invoices, quotes; work:
one product's support, another's sales), so:

- **Per-account flag profiles** (which flags are on, their colour and name) are cheap: an
  optional `[accounts.flags.*]` override of the global table. Limit: Apple Mail names flag
  colours app-wide in its sidebar, so "orange" can mean one thing per Mac, not per account. The
  digest can still use the per-account name.
- **User-defined categories** (name plus a one-line description the model sees, per account)
  fit how people think about mail, but change the prompt per account and work against the Laya
  plan, which needs a fixed label set to fine-tune and calibrate. **Parked (2026-10-28), to
  discuss later:** free-text categories for the general LLM only, or a fixed core set plus
  per-account sub-labels.

**Order (baby steps).**

1. Finish v1 as scoped and run milestone C (a live account in Apple Mail).
2. **v1.1a:** folder discovery and the stored folder list, shown by `check`. No change in
   behaviour.
3. **v1.1b:** per-folder modes `off` / `report` / `classify` / `act` with the `flag` and `digest`
   actions, together with per-account flag profiles. No moves.
4. **v2:** the `move` action with its safety kit (absorbs items 1 and 5 above); per-account
   categories once that decision is made.

Open questions: whether per-folder choices live only in the TOML config or also in a UI (the
design repo has screens for this app); how `undo` presents a batch of moves.

### Other ideas

"Summarize selected message" for Apple Mail via AppleScript + Shortcuts calling
`lets-triage-mail summarize`; an optional Swift MailKit extension for background colours; OAuth2
for Outlook.com/M365 and Gmail.
