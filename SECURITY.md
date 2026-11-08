# Security Policy

## Supported versions

Only the latest release of Kori receives security fixes.

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Report them privately through
[GitHub Security Advisories](https://github.com/darkzmaj/Kori-Discord-Bot/security/advisories/new).
Include the affected version, steps to reproduce and the impact you expect. You should get a reply
within a few days; once a fix is released the advisory is published with credit, unless you prefer
otherwise.

## Operator checklist

- Transcripts contain everything written in a ticket. Keep the transcript log channel private to the
  people who should read them.
- Keep `DISCORD_TOKEN` and webhook URLs out of git. `.env` is ignored; the installer creates it with
  owner-only permissions. If a token leaks, reset it in the Discord Developer Portal immediately.
- Grant Kori only the permissions listed in the README. It never needs Administrator.
- Remember that Discord Administrators bypass channel overwrites: they can read every ticket.
  Give Administrator only to people who should.
- Back up the `kori-data` volume (or `DATABASE_PATH`): it holds ticket records (member and
  channel IDs), so store it as carefully as any other user data.
- Update regularly (re-run the installer, or `docker compose pull && docker compose up -d`).
