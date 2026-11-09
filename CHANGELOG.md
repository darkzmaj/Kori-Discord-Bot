# Changelog

All notable changes to Kori are documented here. Kori follows [Semantic Versioning](https://semver.org/).

## 2.0.0 — 2026-11-09

- **Logging Setup.** The first `/ticket` in a server asks its owner how Kori logs: **Global** (every server's logs go
  to one log server, each showing where it came from, with optional server-specific channels there) or
  **Per server** (logs stay in that server). A public bot is always per server.
- All log channels are picked in `/ticket` → **Logs**. The `LOG_*` and `TRANSCRIPT_*` webhook settings are gone:
  remove them from `.env`.
- Log delivery goes through the bot itself, in order, with no webhooks to create or keep secret.
- Optional health check for uptime monitors (`KORI_HEALTH_ADDR`, for example `0.0.0.0:8080`): `GET /health`
  (or `/`, for proxies that strip a path prefix) answers `ok` (200) only while Kori is connected to Discord, and
  `disconnected` (503) otherwise. Off when unset. The Docker image and compose file expose port 8080.

## 1.0.0 — 2026-11-08

First release of Kori, the ticket and support bot.

- Ticket dashboard (`/ticket`): Setup (status and category, with one-click private category creation),
  Staff Roles, Ticket Types and Panel.
- Panel editor inside `/ticket`: title, description, colour, image and footer. The posted panel updates in place
  whenever the embed or the ticket types change; no resending. A ticket-type selector, or a single Open Ticket button.
- Editable ticket welcome message (title and description, with `{user}` and `{type}` placeholders) and switches
  for pinging the ticket creator and all staff roles.
- Per-server log and transcript channels in `/ticket` → **Logs**. A server's ticket activity and transcripts stay
  in that server; the owner's `.env` webhooks only take bot health and their own server's events.
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
