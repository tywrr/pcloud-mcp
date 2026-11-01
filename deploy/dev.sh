#!/usr/bin/env bash
# Runs the server locally against a local rclone rcd, the way the pod runs them (mise run dev).
#
#   deploy/dev.sh                 serve fixtures/webdav-root, the sample tree
#   deploy/dev.sh DIR             serve a local folder
#   deploy/dev.sh REMOTE:PATH     serve an rclone remote you have configured (e.g. a pcloud remote)
#
# The server reads deploy/k8s/base/config.toml (127.0.0.1:8080, rclone at 127.0.0.1:5572, remote
# "pcloud"); $CONFIG picks another file. rclone defines the remote "pcloud" as an alias of what you
# serve, so the config is the same as in the cluster. Dev secrets: INTERNAL_TOKEN=dev-internal-token
# (what scripts/smoke-test.sh sends); RC_PASS is random per run. Both can be overridden from the env.
# The pod's state directory (/var/lib/chance-of-files) becomes a scratch directory per run, so the
# index works here too and starts empty each time; set $STATE_DIR to keep one between runs.
set -euo pipefail
cd "$(dirname "$0")/.."

src=${1:-fixtures/webdav-root}
case $src in
  *:*) ;;                                   # an rclone remote
  *) [ -d "$src" ] || { echo "not a folder: $src" >&2; exit 1; }
     src=$(realpath "$src") ;;              # absolute, so rclone can't read a colon as a remote
esac
command -v rclone >/dev/null || { echo "rclone not found (mise use -g rclone)" >&2; exit 1; }

export INTERNAL_TOKEN=${INTERNAL_TOKEN:-dev-internal-token}
export RC_PASS=${RC_PASS:-$(openssl rand -hex 32)}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/chance-of-files-dev.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
state=${STATE_DIR:-$scratch/state}
mkdir -p "$state"
# The config with the pod's paths pointed at the scratch state directory.
config=$scratch/config.toml
sed "s|/var/lib/chance-of-files|$state|g" "${CONFIG:-deploy/k8s/base/config.toml}" >"$config"

RCLONE_CONFIG=/dev/null RCLONE_CONFIG_PCLOUD_TYPE=alias RCLONE_CONFIG_PCLOUD_REMOTE="$src" \
  RCLONE_RC_USER=core RCLONE_RC_PASS="$RC_PASS" \
  rclone rcd --rc-addr=127.0.0.1:5572 --rc-serve --tpslimit=8 &
rc=$!
trap 'kill "$rc" 2>/dev/null || true; rm -rf "$scratch"' EXIT

echo "serving $src; MCP at http://127.0.0.1:8080/mcp (Authorization: Bearer $INTERNAL_TOKEN); state in $state" >&2
cargo run --bin chance-of-files -- --config "$config"
