# Kori

[![CI](https://github.com/darkzmaj/Kori-Discord-Bot/actions/workflows/ci.yml/badge.svg)](https://github.com/darkzmaj/Kori-Discord-Bot/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Version](https://img.shields.io/badge/version-1.0.0-green.svg)

Kori is a self-hosted, multi-server Discord ticket and support bot. Each server's configuration is kept separate. You get a fully customisable ticket panel, private ticket channels, staff claiming and admin takeover. Kori is written in Rust, needs no Administrator permission, and is configured almost entirely from inside Discord.

## Quick start

1. Create an application at the [Discord Developer Portal](https://discord.com/developers/applications). Under **Bot**, click **Reset Token** and copy the token, then turn on **Message Content Intent** so ticket transcripts include message text.
2. Run the installer on any machine with [Docker](https://docs.docker.com/get-docker/) and git.

   **Linux / macOS / WSL**
   ```bash
   curl -fsSL https://raw.githubusercontent.com/darkzmaj/Kori-Discord-Bot/main/install.sh | bash
   ```
   **Windows (PowerShell)**
   ```powershell
   irm https://raw.githubusercontent.com/darkzmaj/Kori-Discord-Bot/main/install.ps1 | iex
   ```
   The installer asks for your token and application ID, and saves them to `.env`. It then pulls the prebuilt image (or builds it) and starts Kori. If you gave an application ID, it prints an invite link at the end.
3. Invite Kori (see [Discord setup](#discord-setup)) and run `/ticket` in your server.

**Update:** re-run the same command. **Logs:** `docker compose logs -f`. **Stop:** `docker compose down`. Run these in the install folder, which defaults to `~/kori` (set `KORI_DIR` to change it). Your data lives in the `kori-data` Docker volume and survives updates.

## Setting up tickets

Everything happens in `/ticket`, an ephemeral dashboard with native Discord pickers. Changes are saved immediately.

0. **Logging Setup.** The first time `/ticket` runs in a server, it asks how Kori should log: **Global** or **Per server** (see [Logs](#logs)). Only the server owner can answer. Every server Kori joins goes through this once.
1. **Setup.** Click **Create Category** to make a private `Tickets` category, or use **Set Category** to pick an existing one. You can pause new tickets at any time with **Disable**.
2. **Staff Roles.** Add as many roles as you like whose members can see new tickets, claim them and manage members. The picker takes up to 25 roles at a time; pick again to add more. Discord Administrators always count as staff.
3. **Ticket Types** (optional). Add up to 25 types, such as *Support* or *Billing*, each with an optional description and emoji. With types, the panel shows a selector and channels are named `billing-username`. Without types, the panel shows one **Open Ticket** button and channels are named `ticket-username`.
4. **Panel.** Click **Edit Embed** to set the panel's title, description, colour, image and footer; the form shows the current values. Check it with **Preview**, then post it with **Send Panel**. From then on, every change updates the posted panel in place, with no resending: embed edits, and adding, editing or removing ticket types. **Move Panel** posts it to another channel and removes the old one. **Reset Embed** returns to Kori's default.
5. **Ticket Welcome** (optional). Edit the title and description of the message Kori posts at the top of every new ticket. `{user}` mentions the ticket creator and `{type}` inserts the ticket type. Switch **Ping Creator** and **Ping Staff Roles** on or off to choose who that message notifies. **Preview** shows it without pinging anyone, and **Reset Message** returns to the default.
6. **Logs**. Pick a private staff channel for ticket activity and one for closed-ticket transcripts (in global mode, on the log server). Kori posts a confirmation in each to prove it can write there.

`/ticket` opens on an overview of the whole current setup: status, category, staff roles, ticket types, where the panel is posted (with a jump link) and how many tickets are open. Everything can be changed afterwards: toggle tickets, change or remove the category, add or remove staff roles, add, edit or remove ticket types, and edit or move the panel.

## How tickets work

- **Opening.** A member picks a type (or clicks **Open Ticket**). Kori creates a private channel in the ticket category and pings the member and the staff roles. Each member can have one open ticket per server; this is enforced in the database.
- **Permissions.** `@everyone` cannot see the channel. The opener and added members can view the channel, read history, send messages, attach files, embed links, add reactions, and use external emojis and stickers. Nothing else is allowed: `@everyone`/`@here` mentions, threads, TTS, polls, voice messages, external apps, invites and slash commands are explicitly denied. Staff roles get the same set plus slash commands, so they can use `/add` and `/remove`.
- **Cooldown.** Every ticket channel has a 5-second slowmode.
- **Claiming.** A staff member clicks **Claim**. The staff roles lose access, so only the participants and the claimer remain. Claims are atomic: if two people click at once, only one wins.
- **Reclaiming.** `/reclaim` (Administrator only) makes the administrator the claimer and removes the previous claimer's access.
- **Members.** `/add` and `/remove` work inside a ticket. They can be used by the claimer, by an administrator, or by any staff member while the ticket is unclaimed. The opener and the claimer always keep access, and bots cannot be added.
- **Closing.** The **Close** button opens a form with an optional reason; submitting it is the confirmation. `/close` takes an optional `reason` too. The opener, the claimer, administrators, and staff on an unclaimed ticket can close it. The channel is deleted 5 seconds later. If a ticket channel is deleted by hand, its record closes automatically, including while Kori is offline.
- **Transcripts.** Before deleting the channel, Kori saves every message to a `.txt` transcript. The transcript lists the ticket title, the channel, who opened, claimed and closed it, the reason, all participants (opener, claimer, added members and everyone who wrote), and when the ticket was created and deleted. Kori posts it, with a summary, to the transcript channel chosen in `/ticket` → **Logs**, and sends the same summary and transcript to the opener by DM. Transcripts keep the latest 10,000 messages and stay under 8 MB.

Discord Administrators bypass channel overwrites, so they can always read every ticket. This is how Discord works and Kori cannot change it.

## Configuration

Everything ticket-related is configured per server with `/ticket`. Operator-level settings live in `.env` (see [`.env.example`](.env.example)). Edit `.env`, then re-run the installer or `docker compose up -d`.

| Variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `DISCORD_TOKEN` | yes | — | Bot token. |
| `APPLICATION_ID` | no | — | Safety check that the token belongs to this application. |
| `DATABASE_PATH` | no | `./data/kori.sqlite` | SQLite file (fixed to `/app/data/kori.sqlite` in Docker). |
| `DIAGNOSTIC_GUILD_ID`, `DIAGNOSTIC_USER_ID` | no | — | Extra detail for `rake commands:inspect`. |
| `KORI_HEALTH_ADDR` | no | off | Health check for uptime monitors, for example `0.0.0.0:8080`. `GET /health` (or `/`) answers `ok` (200) only while Kori is connected to Discord, `disconnected` (503) otherwise. |

Kori checks its configuration at startup. If something is wrong, such as a missing token, it exits with a clear message. There are no log settings here: log channels are chosen in Discord.

## Logs

Kori logs into Discord channels you pick from menus; there are no webhooks to create. The first time `/ticket` runs in a server, the server owner chooses how that server logs:

- **🌐 Global:** logs from every server in global mode go to one **log server**, and each log shows which server it came from. Bot health (started, shutting down, startup failed, joined or left a server) goes there too. The first server to choose Global becomes the log server; its owner sets the default channels and can give any server its own channels there with **Server Channels**. Another server in global mode can take over as log server with **Make This the Log Server**, but moving it away from an existing log server takes that server's owner. Choose this when Kori is your own bot.
- **🏠 Per server:** the server's logs stay in that server, in channels it picks itself. Nothing is shared.

If the Discord application is public (**Public Bot** is on in the Developer Portal, so anyone can invite it), Global is not offered and every server is per server: everyone who adds it effectively has their own bot. Kori reads that switch when it starts.

Each mode has two channels:

- **Activity log:** every ticket opened, claimed, reclaimed, closed (with the reason) and deleted (with transcript and DM delivery status); members added and removed; and every `/ticket` configuration change.
- **Transcripts:** one message per closed ticket with its summary and the `.txt` transcript attached.

Kori saves a channel only after it has posted a confirmation there, so a channel it cannot write to is refused straight away. Keep these channels private to staff: transcripts contain every ticket message. The owner can change the mode at any time in `/ticket` → **Logs** → **Change Mode**. Process diagnostics still go to stdout as JSON for your container or service manager.

## Running from source

Kori is written in **Rust** and **Ruby**:

- `src/` (Rust, serenity + rusqlite + tokio) is the bot itself: gateway events, slash commands, the ticket dashboard and lifecycle, transcripts and logging.
- `ruby/` (Ruby) is the operations tooling: migrations, command synchronization and command diagnostics, driven by the `Rakefile`.
- `db/migrations/*.sql` and `shared/commands.json` are shared. The Rust bot embeds them at compile time, the Ruby tools read them at run time, and both compute the same command-schema fingerprint.

1. Install Rust 1.85+ (rustup) and Ruby 3.1+ with Bundler.
2. Run `bundle install`.
3. Copy `.env.example` to `.env` and add `DISCORD_TOKEN`.
4. Run `rake migrate` (the bot also migrates on startup).
5. Use `rake dev` (`cargo run`) for development, or `rake start` (release build) for production.

SQLite runs in WAL mode with foreign keys enabled; use a persistent, backed-up disk volume in production. Kori always bulk-overwrites its six global commands. It then removes legacy guild-scoped Kori commands once per command-schema fingerprint.

## Discord setup

Turn on **Message Content Intent** under Bot in the Developer Portal. Kori reads message text only when it saves a ticket's transcript, and receives no message events otherwise. Install Kori with an OAuth2 URL that has both the `bot` and `applications.commands` scopes; the second is needed for slash commands to appear. Request these bot permissions: View Channels, Manage Channels, Manage Roles, Send Messages, Embed Links, Attach Files, Read Message History, Add Reactions, Use External Emojis, Use External Stickers and Use Application Commands (permissions integer `139855252560`). Kori does not need Administrator.

`Manage Channels` creates and deletes ticket channels and the ticket category. `Manage Roles` is Discord's name for editing channel permission overwrites; Kori uses it only on ticket channels and never assigns roles. Kori grants itself Manage Permissions on each ticket channel, so it can apply the strict participant permission set even for bits it doesn't hold server-wide. If commands are missing, check that the token, the authenticated application ID and the OAuth application all belong to the same application.

## Commands and permissions

Administrators are checked at runtime against Discord's actual Administrator permission. Ticket staff membership is checked at runtime from the member's current roles.

| Command | Access |
| --- | --- |
| `/ticket` (ticket dashboard) | Administrator |
| `/reclaim` | Administrator, inside a ticket |
| `/add`, `/remove` | Claimer, Administrator, or staff while unclaimed, inside a ticket |
| `/close` | Opener, claimer, Administrator, or staff while unclaimed, inside a ticket |

`/ticket` takes no options. Every dashboard control checks Administrator again and refuses controls that belong to a different server.

## Storage model

Everything guild-specific is keyed by `guild_id`: settings, staff roles, ticket types, tickets and their added members, and the panel embed and its posted message. A server can never see or change another server's setup, types or tickets. When Kori is removed from a server, it purges all of that server's data. Inviting Kori again starts from scratch.

## Operations and troubleshooting

Kori emits structured JSON logs and leaves secrets out of them. Do not commit `.env` or database files.

- If ticket creation fails, check Manage Channels and Manage Roles, and that the category has fewer than 50 channels (Discord's limit).
- If the panel cannot be sent, check View Channel, Send Messages and Embed Links in that channel.
- If the ticket category or a staff role is deleted, Kori clears it from the configuration instead of crashing.

Run `rake build`, `rake lint` and `rake test` (Rust and Ruby tests) before deploying. Kori synchronizes the current command schema when it becomes ready. To force a resynchronization in production, run `rake commands:sync`; `rake commands:sync:dry` prints and validates the offline schema without a token. For a read-only diagnosis, run `rake commands:inspect`. It never changes Discord state.

## Production operations

Use persistent storage. Run `bundle install`, `rake migrate` and `rake build`, then run `target/release/kori`. Kori exits non-zero when required configuration is missing. It handles `SIGINT`/`SIGTERM` by flushing the audit log and disconnecting from Discord. Run it under systemd, Docker or another supervisor that restarts it after non-zero exits.

Back up the SQLite database while the process is stopped, or use SQLite's online backup tooling so the WAL file is included consistently. Logs are JSON written to stdout, so collect them through your process manager.

### Deploying with Docker / Coolify

`Dockerfile` builds a multi-stage, non-root production image: a Rust build stage, then a Ruby 3.4 slim runtime with the `kori` binary and the Ruby tooling. `docker-compose.yml` runs it with a named volume for `/app/data`, a read-only root filesystem, dropped capabilities and `no-new-privileges`. Migrations run automatically on startup and slash commands resynchronize on ready, so no extra setup is needed beyond the environment variables.

In Coolify, create a new resource from this repository with **Docker Compose** as the build pack. Then set `DISCORD_TOKEN` (and optionally `APPLICATION_ID`) under Environment Variables. Do not set `DATABASE_PATH`. Kori only makes outbound connections to the Discord gateway, so no port is needed. For an uptime monitor, set `KORI_HEALTH_ADDR=0.0.0.0:8080` and route a domain or path to container port 8080. The check also answers at `/`, so a proxy that strips its path prefix (such as `https://example.com/kori-health`) works; have the monitor look for the keyword `ok`.

Inside the container, the tooling is available as `bundle exec rake commands:sync`, `commands:inspect` and `migrate`.

## Contributing and security

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md). Release notes are in [CHANGELOG.md](CHANGELOG.md).

## License

Kori is released under the [MIT License](LICENSE). Copyright (c) 2026 DarkZmaj.

It includes a lightly patched copy of [serenity](https://github.com/serenity-rs/serenity) in `vendor/serenity`, distributed under its own ISC license ([`vendor/serenity/LICENSE.md`](vendor/serenity/LICENSE.md)).
