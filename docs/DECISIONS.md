# Decision log

| Date | Decision | Why |
|---|---|---|
| 2026-10-24 | Don't adopt Inbox Zero; build our own | Inbox Zero only supports Gmail and Outlook; Piotr's accounts are other IMAP providers. It's otherwise trustworthy (see RESEARCH.md). |
| 2026-10-24 | Email is processed **only by a local model** | Privacy: no email content goes to cloud AI. |
| 2026-10-24 | Work **server-side over IMAP**, not inside a mail client | The results show in Apple Mail, on the iPhone and in webmail; there's nothing to install in Mail; it works while Mail is closed. |
| 2026-10-24 | Python prototype built and tested (34 tests) | Proved the design; it is now the reference spec. Kept out of the public repo so the history starts with Rust. |
| 2026-10-25 | Rewrite in **Rust** | Piotr wants to learn Rust; a small, well-bounded daemon is a good learning project; single-binary distribution suits open source. |
| 2026-10-25 | **Open-source** GitHub project, aiming for stars | Useful to people on the same stack (Mac, Apple Mail, local LLM, self-hosted IMAP). |
| 2026-10-25 | Use **flag colours (RFC 9979 keywords)** for visual triage, not a MailKit extension | Flags are stored on the server, sync to the iPhone, need no Swift/Xcode/signing. Background colour needs a MailKit extension: maybe later, optional. |
| 2026-10-25 | **Colour = action needed, not category**; red reserved for the user | Every coloured message is also flagged and lands in the Flagged mailbox. |
| 2026-10-25 | **v1 = flagging + daily digest email only** | Smallest useful version. Moves, unsubscribe and the web UI come later. |
| 2026-10-25 | Digest delivered via **IMAP APPEND**, not SMTP | No deliverability issues; it appears in every client. |
| 2026-10-25 | Laya decision model: **v2 candidate**, not v1 (pending confirmation) | Zero-shot accuracy near chance; needs our own labelled data plus calibration; can't do summaries/dates anyway. v1 logs labels to make that possible. See CLASSIFIER.md. |
| 2026-10-25 | Name: **Let's Triage Mail** (`lets-triage-mail`), repo `lets-code-apps/lets-triage-mail` | MailGuard was crowded (~290 GitHub repos). Follows Piotr's "Let's ..." brand (from his company, Let's Code): `lets-<verb>-<object>` for services, `lets-<verb>` for tools. "Triage" covers flagging, digests and later spam filtering and unsubscribing, not just flags. Free on crates.io, Homebrew and GitHub on 2026-10-25. |
| 2026-10-25 | **Post-v1 roadmap**: spam/security filtering to Junk/Trash, unsubscribing, drafted replies, home-assistant integration | The service should handle the whole inbox, and it is one microservice in Piotr's local assistant setup. v1 scope is unchanged. Drafts go to the Drafts folder via APPEND; the service never sends mail. See SPEC.md §5. |
| 2026-10-26 | **Per-account `dry_run`** overrides `[policy] dry_run` | Go live on one low-volume account first, with the others still in dry-run. `flagging = false` still wins. |
| 2026-10-26 | A live account **without custom keywords refuses to start** (fatal at connect, not just in `check`) | A bare `\Flagged` shows red in Apple Mail, and red is the user's. |
| 2026-10-26 | The **auto-clear pass runs in the account worker** on its wake-ups (every 15 min at most), not on a separate timer or connection | One task per mailbox means no two STOREs can race on one message, the connection is already open, and nothing about clearing is urgent. |
| 2026-10-26 | Ownership rows store the **flag kind and the deadline date**; Sent harvest stores **reply ids** | The colour is configurable, so it cannot say whether a flag is "Reply needed" or "Deadline". Ids only, no subjects or bodies. |
| 2026-10-26 | Only "Reply needed" and "Deadline" clear themselves | As in FLAGS.md; "To pay", "Bookkeeping" and "Unsure" wait for the user. A deadline flag stays through the deadline day. An unreadable date never clears. |
| 2026-10-26 | `backfill` never writes the worker's position | A backfill that moved the cursor backwards would re-read mail, forwards would skip new mail. The `processed` table is enough to skip what was handled. |
| 2026-10-26 | IMAP folder names are decoded for display only, by a hand-written decoder | `[Gmail]/Wys&AUI-ane` is unreadable in `check`. The server needs the raw name, so there is no encoder. About 40 lines did not justify a dependency. |
| 2026-10-26 | **The digest is English only** in v1; `[digest] language` other than `en` is a config error | A digest in a language the strings do not exist for is a silent wrong answer. Rejecting at startup is clearer than ignoring with a warning. The key stays so translations can come later. |
| 2026-10-26 | **No model calls for the digest**; each line uses the stored verdict `reason`. Changes PLAN 5.1 | Bodies are never stored, so re-summarising means re-fetching mail and a second model pass per message. The `reason` is already a one-sentence summary written when the mail was classified. |
| 2026-10-26 | A **dry-run account is never appended to**; its digest is a 0600 file under `<data dir>/digests/` (newest 14 kept) | Dry-run promises the mailbox is not written. The file lets the owner preview the digest first. Accounts that are not dry-run, classify-only ones included, get the APPEND: for wp.pl the digest is the main output. |
| 2026-10-26 | The worker skips its own digest **by recorded UID (APPENDUID) and random Message-ID, not by header** | A sender can write `X-Lets-Triage-Mail: digest` and slip past the classifier. The Message-ID is stored before the APPEND (the worker can see the mail before we get the server's answer) and the UID after it. |
| 2026-10-26 | An **empty day still gets a digest** | A silent day cannot be told from a dead service. |
| 2026-10-26 | A digest covers **the time since the previous one, at most 48 h** | Nothing is reported twice or missed; after a long absence the last two days are what is still worth reading. `stats` has the rest. |
| 2026-10-26 | The digest loop **polls the clock every minute** instead of sleeping until the time | A monotonic timer does not advance while the Mac sleeps, so a long sleep would end late. The poll costs one small read and gives catch-up after wake for free. |
| 2026-10-26 | `APPEND` is written by hand in `ImapSession::append_message`, not with `async-imap`'s `append` | That one discards the server's final answer, which holds the `APPENDUID`. Checked against a scripted fake server because Dovecot runs in CI only. |
| 2026-10-26 | `mail-builder` 2 for the message, `getrandom` for the Message-ID | The builder's own ids and boundaries are only pseudo-unique; the digest's Message-ID must be unguessable, since the worker trusts it. |
| 2026-10-28 | The Spam folder is **watched read-only** (`EXAMINE`, headers with `PEEK`, no model) on the inbox worker's connection, every 15 min at most; trusted senders with a passing DKIM are listed in the digest as "Possibly misfiled in Spam" | A real DKIM-valid message from a known sender landed in wp.pl's `Spam` and was never seen. Running the model on a folder of hundreds of junk messages is not worth it for the question "do I know this sender?". No second connection: some providers limit them, and nothing here is urgent. |
| 2026-10-28 | **The app offers capabilities and recommends a setup; the user chooses.** Post-v1 the design is folder → mode (`off` / `report` / `classify` / `act`) → actions (`flag`, `move`, `digest`), with discovered folders configured per account | Accounts and folders are used for different jobs, and model cost with a small decision model is low enough to leave to the user. One model keeps flags, moves and the Spam report on one code path. Proposal and order in SPEC.md §5. v1 scope is unchanged. |
| 2026-10-28 | **Moving mail will be allowed (v2)**, opt-in per folder and category, never for trusted senders, move-only (no delete), logged, with `undo`. **Dry-run is optional**: recommended, not required | Ends the v1 "never move or hide mail" rule after v1, by Piotr's decision. A wrong move hides mail, so the safety kit is not optional; the waiting period is the user's call. |
| 2026-10-28 | **Folders are discovered, not configured**: from the connection details alone the service lists every folder with its role and stores the list per account. Baby steps: v1.1a discovery, v1.1b folder modes together with per-account flag profiles | The user should not have to know IMAP folder names or delimiters. Discovery alone changes no behaviour, so it can ship and be checked first. |
| 2026-10-29 | The digest HTML follows the design repo's email (`design/email/triage-mail/`), with its header image **inside the message** as an inline `cid:` part; "possibly misfiled in spam" uses the section style with a neutral dot until the design covers it | An attached image is not a remote load, so the digest still tells nobody it was opened. The CSS gradient under it covers clients that block it. A user-chosen blue flag gets the neutral dot: there is no blue token yet. |
| 2026-10-29 | The digest header is **one `<img src="cid:…">` with the title drawn into the picture** (title left, emblem right, no tint), the title in its `alt` | Gmail on iOS drops negative margins (live text fell under the picture), drops CSS background images (the picture vanished), and recolours text in dark mode but never images. Forwarding keeps only images an `<img>` uses. The title is drawn at 30 px on the 600 px column (about 17 px on a phone); 44 and 34 px looked too big in Apple Mail. The picture is a draft made here; the design repo's draft uses a CSS background with live text and needs updating. |

## Rejected or parked options

- **Per-account categories** (parked 2026-10-28, to discuss later): free-text categories per
  account for the general LLM, or a fixed core set plus per-account sub-labels. The choice
  depends on the Laya question, which needs a fixed label set. See SPEC.md §5.

- **Names: MailGuard, Mailbird, Bunting, Pennon, Kittiwake, Let's Flag, Let's Guard Email**: crowded or taken (Mailbird is an existing mail client), or too narrow (flag only) or misleading ("guard" suggests a security filter).
- **Spark / Canary / SaneBox**: their AI or filtering runs in their cloud.
- **Hosted Inbox Zero**: cloud LLM; Gmail/Outlook only.
- **Apple Intelligence in Mail**: fine to keep on, but there's no bulk unsubscribe or AI blocking, and some requests go to Private Cloud Compute.
- **Legacy Mail plug-ins (.mailbundle)**: no longer load since macOS Sonoma.
- **MailKit extension for summaries**: impossible. MailKit has no reading-pane or sidebar hook.
  Available hooks: message actions (move to Junk/Trash/Archive, flag, background colour, read
  state; reportedly only sees Inbox deliveries, and only while Mail runs), compose session (a panel
  in the compose window), content blocker, message security.
- **"Summarize selected message" via AppleScript + Shortcuts**: parked for later. Mail's
  AppleScript gives `selection`, `content`, `subject`. The script writes the body to a temp file and
  calls `lets-triage-mail summarize`, then shows a dialog. Trigger it with a Shortcuts keyboard shortcut or a
  Services Quick Action. The first run asks for an Automation permission.
