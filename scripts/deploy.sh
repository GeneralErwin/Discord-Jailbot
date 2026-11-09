#!/usr/bin/env bash
#
# Fast update for the jail bot: one compressed SSH stream instead of dragging
# every file through SFTP, then a release build and a PM2 restart on the server.
#
# Usage (from the project root, in Git Bash):
#   bash scripts/deploy.sh user@server [remote-dir]
#
# Example:
#   bash scripts/deploy.sh root@203.0.113.10 /opt/jailbot
#
# Only the source is sent. `target/`, `data/` and `.git/` are excluded, so a
# typical update is around a hundred kilobytes rather than the whole tree.

set -euo pipefail

target=${1:?usage: bash scripts/deploy.sh user@server [remote-dir]}
remote_dir=${2:-/opt/jailbot}

echo "Uploading source to ${target}:${remote_dir} ..."

tar \
  --exclude=./target \
  --exclude=./data \
  --exclude=./.git \
  --exclude=./.freebuff \
  --exclude=./.vscode \
  -czf - . \
  | ssh "$target" "set -e
      export PATH=\"\$HOME/.cargo/bin:\$PATH\"
      mkdir -p '$remote_dir'
      tar -xzf - -C '$remote_dir'
      cd '$remote_dir'
      cargo build --release
      if command -v pm2 >/dev/null 2>&1; then
        pm2 restart jailbot --update-env
      else
        echo 'Build finished. Restart the bot manually (pm2 not found in this shell).'
      fi"
