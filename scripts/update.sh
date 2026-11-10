#!/usr/bin/env bash
#
# Run this ON THE SERVER, in the bot directory, after pulling or copying source.
#
#   bash scripts/update.sh
#
# Rust is a compiled language: copying .rs files over the old ones does not
# change the running bot, and `pm2 restart` only re-launches the existing
# binary. This rebuilds the release binary, then restarts PM2 so the new build
# is actually what runs.

set -euo pipefail
cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$PATH"

if [ -f data/jail.sqlite3 ]; then
  cp data/jail.sqlite3 data/jail.sqlite3.bak
  echo "Database backed up to data/jail.sqlite3.bak"
fi

echo "Building release binary..."
cargo build --release

bin=target/release/discord-bot
if [ ! -x "$bin" ]; then
  echo "No binary at $bin - the build did not finish." >&2
  exit 1
fi

echo "Built: $(ls -l --time-style=+%Y-%m-%d\ %H:%M:%S "$bin")"

if command -v pm2 >/dev/null 2>&1; then
  pm2 restart jailbot --update-env || pm2 start "$bin" --name jailbot
  pm2 logs jailbot --lines 15 --nostream || true
else
  echo "pm2 not found in this shell - restart the bot manually."
fi
