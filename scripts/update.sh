#!/usr/bin/env bash
#
# Run this ON THE SERVER, in the bot directory, after pulling or copying source.
#
#   bash scripts/update.sh
#
# Rust is compiled: copying .rs files does not change the running bot, and
# `pm2 restart` alone only re-launches the existing binary. This rebuilds the
# release binary, then restarts PM2 so the new build is what actually runs.

set -euo pipefail
cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$PATH"

bin=target/release/discord-bot

if [ -f data/jail.sqlite3 ]; then
  cp data/jail.sqlite3 data/jail.sqlite3.bak
  echo "Database backed up to data/jail.sqlite3.bak"
fi

# Find the PM2 process that is already running this binary, so the process name
# never has to be guessed. Falls back to any single registered process.
find_app() {
  local found
  found=$(pm2 jlist 2>/dev/null \
    | sed 's/},{/}\n{/g' \
    | grep -iF 'discord-bot' \
    | grep -o '"name":"[^"]*"' \
    | head -1 | cut -d'"' -f4 || true)
  if [ -n "$found" ]; then
    echo "$found"
    return
  fi
  found=$(pm2 jlist 2>/dev/null \
    | sed 's/},{/}\n{/g' \
    | grep -o '"name":"[^"]*"' \
    | cut -d'"' -f4 | head -1 || true)
  echo "$found"
}

echo "Building release binary..."
cargo build --release

if [ ! -x "$bin" ]; then
  echo "No binary at $bin - the build failed, nothing was restarted." >&2
  exit 1
fi

echo "Built: $(ls -l --time-style=+%Y-%m-%d\ %H:%M:%S "$bin")"

if ! command -v pm2 >/dev/null 2>&1; then
  echo "pm2 not found in this shell - restart the bot manually." >&2
  exit 0
fi

app=$(find_app)
if [ -n "$app" ] && pm2 describe "$app" >/dev/null 2>&1; then
  echo "Restarting PM2 process: $app"
  pm2 restart "$app" --update-env
  pm2 logs "$app" --lines 15 --nostream || true
else
  echo "No existing process found - starting a new one named jailbot"
  pm2 start "$bin" --name jailbot --cwd "$(pwd)"
  pm2 logs jailbot --lines 15 --nostream || true
fi
