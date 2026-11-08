# Changelog

All notable changes to Kori are documented here. Kori follows [Semantic Versioning](https://semver.org/).

## 1.0.0 — 2026-11-08

First release of Kori, the ticket and support bot.

- Ticket dashboard (`/ticket`): Setup (status and category, with one-click private category creation),
  Staff Roles, Ticket Types and Panel.
- Panel editor inside `/ticket`: title, description, colour, image and footer. The posted panel updates in place
  whenever the embed or the ticket types change; no resending. A ticket-type selector, or a single Open Ticket button.
- Editable ticket welcome message (title and description, with `{user}` and `{type}` placeholders) and switches
  for pinging the ticket creator and all staff roles.
- `/ticket` overview shows the whole current setup; every existing setting can be edited (types included).
- Private ticket channels named `type-username` (or `ticket-username`), hidden from `@everyone`, with a
  5-second message cooldown and a strict permission set (view, send, history, attach, embed links,
  reactions, external emojis and stickers; nothing else).
- Claiming: once claimed, staff roles lose access and only the participants and the claimer remain.
  `/reclaim` lets an administrator take over.
- `/add` and `/remove` members. `/close` or the Close button, each with an optional reason.
- Transcripts: every closed ticket is saved as a `.txt` (title, opened/claimed/closed by, reason, participants,
  creation and deletion time, every message), posted to the transcript log and sent to the opener by DM.
- Two optional log destinations: the main log (member actions, staff actions, bot health) and the transcript log.
  Each webhook is verified against its guild and channel IDs. Startup fails if a log is only partly configured.
- No limit on ticket staff roles.
- One open ticket per member per server, enforced in the database.
- One-command install (`install.sh`, `install.ps1`) and prebuilt multi-arch Docker images.
- Message Content Intent is used only to read a ticket's messages for its transcript.
