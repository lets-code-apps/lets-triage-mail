# Flag colours

## How Apple Mail stores flag colours (RFC 9979)

The colour is stored **on the IMAP server**, so it shows in Mail on the Mac, on the iPhone and in
other clients that understand it. It is encoded as the `\Flagged` system flag plus three keywords:

| `$MailFlagBit0` | `$MailFlagBit1` | `$MailFlagBit2` | Colour |
|---|---|---|---|
| 0 | 0 | 0 | red |
| 1 | 0 | 0 | orange |
| 0 | 1 | 0 | yellow |
| 1 | 1 | 0 | green |
| 0 | 0 | 1 | blue |
| 1 | 0 | 1 | purple |
| 0 | 1 | 1 | gray |

- The bits **must be ignored when `\Flagged` is not set**. Clients must not set the bits unless
  `\Flagged` is set in the same action or already set.
- 1-1-1 has no colour assigned.
- The server must allow custom keywords: `PERMANENTFLAGS` contains `\*`. Check this in
  `check`/`doctor`. Dovecot, Fastmail and iCloud allow it.
- Consequence: **every coloured message is also flagged** and appears in Mail's "Flagged"
  mailbox. Colours therefore mean "action needed", not "category".
- Flag names can be renamed in the Mac Mail sidebar (expand Flagged) so "Orange" reads
  "Reply needed". The iPhone may still show colour names.

Source: https://datatracker.ietf.org/doc/rfc9979/

## Scheme (agreed 2026-10-25)

| Bits | Colour | Name | Set when | Auto-cleared when | Default |
|---|---|---|---|---|---|
| 000 | red | **Mine** | never: reserved for the user | never touched | n/a |
| 100 | orange | **Reply needed** | a real person asks something / is waiting on the user | the user replies (a matching message appears in Sent) | on |
| 010 | yellow | **Deadline** | the message has a date to act by (expiring, appointment, renewal, form) | the date has passed | off |
| 110 | green | **To pay** | unpaid invoice, bill, payment request | the user clears it, or a matching "payment received" arrives | off (Piotr: on) |
| 101 | purple | **Bookkeeping** | paid invoice or receipt needed for accounting (VAT invoices for Piotr's business) | the user clears it | off (Piotr: on) |
| 001 | blue | **Waiting on them** | set on the user's **sent** message when no reply has arrived after N days | their reply arrives | off |
| 011 | gray | **Unsure** | the model was not confident (below threshold) | the user acts on it | on |

Rules:
- **One colour per message.** Priority: Reply needed > Deadline > To pay > Bookkeeping > Unsure.
  (Waiting on them lives on sent messages, so it never competes.)
- **Never set red.** Default manual flagging in Mail is red, so the user's own flags stay distinguishable.
- **Ownership:** record every flag the service sets (account, mailbox, UIDVALIDITY, UID, Message-ID,
  colour, reason, timestamp). Before clearing or changing a flag, re-read the current flags. If
  they differ from what the service set (the user changed the colour, unflagged it or re-flagged it),
  mark the message as user-owned and never touch it again.
- Never flag a message the user already flagged.
- Names, colours and enabled state live in config, because other users will want different meanings.
- Flag noise is the main risk. Keep the defaults minimal (Reply needed and Unsure).

## What the model must return to drive this

Beyond `category`:
- `needs_reply`: bool, with confidence
- `deadline`: ISO date or null (needs a generative model; see CLASSIFIER.md)
- `payment`: `unpaid` | `paid` | `none`

"Waiting on them" needs no model: it watches Sent and matches replies via
`In-Reply-To`/`References`.

## Background colour (not v1)

Row background colour in the message list (without flagging) is only possible from a **MailKit
extension** (Swift, `MEMessageAction.setBackgroundColor`). It is local to that Mac, is not stored
on the server, and works only while Mail runs. It could make a later optional add-on for
category tinting.
