# Research notes (2026-10-24/25)

## Inbox Zero (github.com/elie222/inbox-zero)

- AI email **assistant** layered on Gmail/Outlook (not a client): labels, drafts, bulk unsubscribe,
  cold-email blocker, analytics. About 11.7K stars, very frequent releases, largely one maintainer.
- Trust signals: open source; hosted version claims SOC 2 Type 2 (via Comp AI); restricted Gmail
  scopes imply an annual CASA assessment for the hosted app.
- One CVE: CVE-2026-42865 (fixed in 2.29.3). A shared Redis listener could leak one account's
  cleaner-stream thread events to another account. Low severity; disclosed properly.
- Self-hosting supports Ollama. The licence has a self-hosting limit (free up to five business users).
- **Doesn't fit Piotr:** Gmail/Outlook only.
- Useful as inspiration: plain-English rules, cold-email blocker, bulk unsubscribe UX, digest.

## Mac clients considered

- Apple Mail: Apple Intelligence categories (Primary/Transactions/Updates/Promotions), summaries;
  built-in unsubscribe banner.
- Spark: Gatekeeper (screen new senders); AI in Spark's cloud (Google Cloud).
- Canary: AI Copilot, 1-click unsubscribe, PGP; cloud AI.
- SaneBox: server-side IMAP filter (SaneLater, SaneBlackHole); about $59/yr; sorts on
  headers/behaviour, not bodies; cloud service.
- Conclusion: none are local-LLM. The SaneBox model (server-side folders work in any client) is the
  right shape, which is what we built.

## Apple Mail extension APIs

- Legacy plug-ins are dead since macOS Sonoma. MailKit (macOS 12+) only.
- MailKit capabilities: `MEMessageActionHandler`, `MEComposeSessionHandler`, `MEContentBlocker`,
  `MEMessageSecurityHandler`.
- `MEMessageAction`: `markAsRead`, `markAsUnread`, `moveToArchive`, `moveToJunk`, `moveToTrash`,
  `flag(_:)`, `setBackgroundColor(_:)`.
- No reading-pane or summary view is possible.
- Rust bindings exist (`objc2-mail-kit`), but an extension should be written in Swift if we ever
  build one.

## Flag colours

- RFC 9979 (May 2026, Informational) registers `$MailFlagBit0..2`. See FLAGS.md.

## Laya / JEV

See CLASSIFIER.md. Laya: huggingface.co/convaiinnovations/laya, Apache 2.0, September 2026.

## Unsubscribe (for later versions)

- RFC 8058 one-click: requires `List-Unsubscribe-Post: List-Unsubscribe=One-Click`, an HTTPS URI,
  and a DKIM-signed message. POST body `List-Unsubscribe=One-Click`.
- Never "unsubscribe" from spam or phishing: it confirms the address is live.
