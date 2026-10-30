#!/usr/bin/env bash
# Serves a folder read-only over WebDAV with rclone, the way the laptop serves a subfolder
# of the pCloud Drive mount in production. With no argument it serves fixtures/webdav-root.
#
#   scripts/dev-backend.sh                          # the sample tree, for local development
#   RCLONE_PASS=... scripts/dev-backend.sh ~/pCloudDrive/Library
#
# Environment:
#   RCLONE_USER   Basic auth user (default mcp); must match the Worker's BACKEND_USER.
#   RCLONE_PASS   Basic auth password (default dev-password); must match BACKEND_PASSWORD.
#   BACKEND_ADDR  Address to listen on (default 127.0.0.1:8081, which matches
#                 crates/worker/.dev.vars.example). Keep it on loopback. The production rclone
#                 (deploy/systemd) listens on 8080, so both can run on one machine.
#
# rclone reads RCLONE_USER and RCLONE_PASS itself, so the password never appears in `ps`.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dir="${1:-$repo_root/fixtures/webdav-root}"
addr="${BACKEND_ADDR:-127.0.0.1:8081}"

if ! command -v rclone >/dev/null 2>&1; then
  echo "rclone not found; install it from https://rclone.org/install/ (or: mise use -g rclone)" >&2
  exit 1
fi
if [[ ! -d "$dir" ]]; then
  echo "not a folder: $dir" >&2
  exit 1
fi

export RCLONE_USER="${RCLONE_USER:-mcp}"
export RCLONE_PASS="${RCLONE_PASS:-dev-password}"

echo "Serving $dir read-only at http://$addr (user $RCLONE_USER)" >&2
# An absolute path stops rclone from reading a folder name with a colon in it as a remote
# such as "pcloud:".
exec rclone serve webdav "$(realpath "$dir")" --addr "$addr" --read-only
