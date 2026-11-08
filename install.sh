#!/usr/bin/env bash
# Kori one-command installer and updater (Linux, macOS, WSL). Requires Docker with Compose v2.
#   curl -fsSL https://raw.githubusercontent.com/darkzmaj/Kori-Discord-Bot/main/install.sh | bash
# Re-run it at any time to update. Set KORI_DIR to choose the install folder.
set -euo pipefail

REPO_URL="https://github.com/darkzmaj/Kori-Discord-Bot.git"
DIR="${KORI_DIR:-$HOME/kori}"
PERMISSIONS=139855252560

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || die "Docker is required: https://docs.docker.com/get-docker/"
docker compose version >/dev/null 2>&1 || die "Docker Compose v2 is required (the 'docker compose' command)."

# Use the current checkout if run from one, otherwise clone or update $DIR.
if [ -f docker-compose.yml ] && [ -f .env.example ] && grep -q 'kori' docker-compose.yml; then
  DIR=$PWD
elif [ -d "$DIR/.git" ]; then
  say "Updating $DIR"
  git -C "$DIR" pull --ff-only
else
  command -v git >/dev/null 2>&1 || die "git is required."
  say "Downloading Kori into $DIR"
  git clone --depth 1 "$REPO_URL" "$DIR"
fi
cd "$DIR"

# Replace KEY=... in .env, keeping every other line.
set_key() {
  awk -v k="$1" -v v="$2" 'BEGIN {FS = OFS = "="} $1 == k {print k "=" v; done = 1; next} {print} END {if (!done) print k "=" v}' .env > .env.tmp
  mv .env.tmp .env
}

ask() { # ask VAR "Prompt" default regex error
  local answer
  while :; do
    read -rp "$2 [$3]: " answer </dev/tty || die "No input."
    answer=${answer:-$3}
    [[ $answer =~ $4 ]] && break
    echo "  $5" >&2
  done
  printf -v "$1" '%s' "$answer"
}

app_id=""
if [ ! -f .env ]; then
  # Prompts read the terminal directly so this also works when piped from curl.
  [ -r /dev/tty ] || die "No terminal. Copy .env.example to .env, set DISCORD_TOKEN, then re-run."
  say "First-time setup. Press Enter to keep the [default]."
  echo "  Token: https://discord.com/developers/applications -> your app -> Bot -> Reset Token"
  echo "  Also turn on 'Message Content Intent' on that page (needed for ticket transcripts)."
  echo "  Log channels (optional) are set in .env afterwards: see .env.example."
  while :; do
    read -rsp "Discord bot token: " token </dev/tty || die "No input."
    echo
    [[ $token =~ ^[A-Za-z0-9._-]{50,}$ ]] && break
    echo "  That does not look like a bot token." >&2
  done
  ask app_id "Application ID (General Information page, optional)" "" '^([0-9]{17,20})?$' "Use the numeric ID, or leave it empty."

  umask 077
  cp .env.example .env
  set_key DISCORD_TOKEN "$token"
  set_key APPLICATION_ID "$app_id"
  chmod 600 .env
  unset token
  say "Saved settings to $DIR/.env (edit it any time, then re-run this script)."
else
  app_id=$(sed -n 's/^APPLICATION_ID=//p' .env | tr -d '[:space:]')
fi

say "Fetching the Kori image"
if ! docker compose pull kori 2>/dev/null; then
  say "No prebuilt image for this platform; building locally (the first build takes several minutes)."
  docker compose build kori
fi
docker compose up -d --remove-orphans

say "Kori is running."
echo "  Logs:    cd \"$DIR\" && docker compose logs -f"
echo "  Update:  re-run this installer (or: docker compose pull && docker compose up -d)"
echo "  Stop:    cd \"$DIR\" && docker compose down"
if [ -n "$app_id" ]; then
  echo "  Invite:  https://discord.com/oauth2/authorize?client_id=$app_id&scope=bot+applications.commands&permissions=$PERMISSIONS"
fi
