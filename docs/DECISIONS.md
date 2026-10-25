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

## Rejected or parked options

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
