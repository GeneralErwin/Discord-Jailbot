#!/usr/bin/env bash
#
# Run this ON THE SERVER, in the bot directory, after pulling or copying source.
#
#   bash scripts/update.sh
#   PM2_APP=baby bash scripts/update.sh     # force a specific process
#
# Rust is compiled: copying .rs files does not change the running bot, and
# `pm2 restart` alone only re-launches the existing binary. This rebuilds the
# release binary, restarts PM2, and fails if the new build did not come up.

set -euo pipefail
cd "$(dirname "$0")/.."

export PATH="$HOME/.cargo/bin:$PATH"

bin=target/release/discord-bot

if [ -f data/jail.sqlite3 ]; then
  cp data/jail.sqlite3 data/jail.sqlite3.bak
  echo "Database backed up to data/jail.sqlite3.bak"
fi

# Find the PM2 process to restart. Prints:
#   <name>              found it
#   __AMBIGUOUS__:<...> several processes run the bot (duplicate instances)
#   __FALLBACK__:<name> no match, but PM2 holds exactly one app
#   (nothing)           no safe choice, start a new one
find_app() {
  local json matches names count want
  # 1. Explicit override always wins.
  if [ -n "${PM2_APP:-}" ] && pm2 describe "$PM2_APP" >/dev/null 2>&1; then
    printf '%s' "$PM2_APP"
    return
  fi
  json=$(pm2 jlist 2>/dev/null || true)
  # 2. The process whose command line points at the bot binary.
  matches=$(printf '%s' "$json" \
    | sed 's/},{/}\n{/g' \
    | grep -iF 'discord-bot' \
    | grep -o '"name":"[^"]*"' \
    | cut -d'"' -f4 | sort -u || true)
  count=$(printf '%s\n' "$matches" | grep -c . || true)
  if [ "$count" -eq 1 ]; then
    printf '%s' "$matches"
    return
  fi
  if [ "$count" -gt 1 ]; then
    printf '__AMBIGUOUS__:%s' "$matches"
    return
  fi
  # 3. Conventional names, for processes started through npm or a wrapper.
  names=$(printf '%s' "$json" \
    | sed 's/},{/}\n{/g' \
    | grep -o '"name":"[^"]*"' \
    | cut -d'"' -f4 | sort -u || true)
  for want in baby jailbot prisoner-bot discord-bot; do
    if printf '%s\n' "$names" | grep -qxF "$want"; then
      printf '%s' "$want"
      return
    fi
  done
  # 4. Only when PM2 holds a single app, so nothing else gets bounced silently.
  count=$(printf '%s\n' "$names" | grep -c . || true)
  if [ "$count" -eq 1 ]; then
    printf '__FALLBACK__:%s' "$names"
  fi
}

# Confirm the process reached "online" and that its fresh log has no fatal
# error. Returns non-zero so a crashed build is never reported as a success.
verify_boot() {
  local app=$1 i status logs fatal
  status=""
  for i in 1 2 3 4 5 6; do
    status=$(pm2 jlist 2>/dev/null \
      | sed 's/},{/}\n{/g' \
      | grep -F "\"name\":\"$app\"" \
      | grep -o '"status":"[^"]*"' \
      | head -1 | cut -d'"' -f4 || true)
    if [ "$status" = "online" ]; then
      break
    fi
    sleep 2
  done

  logs=$(pm2 logs "$app" --lines 80 --nostream 2>&1 || true)
  fatal=$(printf '%s\n' "$logs" \
    | grep -iE "panicked at|thread 'main' panicked|DISCORD_TOKEN|invalid token|already in use|Cannot allocate memory|(^|\| )Error:" \
    | head -5 || true)

  if [ -n "$fatal" ]; then
    echo "" >&2
    echo "UPDATE FAILED - $app crashed on start-up:" >&2
    printf '%s\n' "$fatal" >&2
    echo "The previous build is no longer running; fix the error and run this again." >&2
    return 1
  fi

  if [ "$status" != "online" ]; then
    echo "" >&2
    echo "UPDATE FAILED - $app is '${status:-unknown}' instead of online." >&2
    printf '%s\n' "$logs" | tail -15 >&2
    return 1
  fi

  echo "Bot is online."
  printf '%s\n' "$logs" | tail -10
  return 0
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

case "$app" in
  __AMBIGUOUS__:*)
    echo "Several PM2 processes are running the bot:" >&2
    printf '%s\n' "${app#__AMBIGUOUS__:}" >&2
    echo "Duplicate instances share one gateway token - stop all but one, then run this again." >&2
    exit 1
    ;;
  __FALLBACK__:*)
    app=${app#__FALLBACK__:}
    echo "Warning: no PM2 process points at $bin." >&2
    echo "Restarting the only registered app instead: $app" >&2
    ;;
esac

if [ -n "$app" ] && pm2 describe "$app" >/dev/null 2>&1; then
  echo "Restarting PM2 process: $app"
  # Clear the old log so a panic from an earlier run cannot be blamed on this one.
  pm2 flush "$app" >/dev/null 2>&1 || true
  pm2 restart "$app" --update-env
  verify_boot "$app" || exit 1
else
  echo "No bot process found in PM2 - starting a new one named baby"
  pm2 start "$bin" --name baby --cwd "$(pwd)"
  verify_boot baby || exit 1
fi
