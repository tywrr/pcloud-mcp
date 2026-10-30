#!/usr/bin/env bash
# Deploys the pcloud-mcp backend (rclone + cloudflared, backend.yaml) to the cluster in $KUBECONFIG.
#   deploy/k8s/apply.sh            create the Secret if missing, apply, wait for the tunnel
#   deploy/k8s/apply.sh --rotate   replace the Secret from the files below, then restart the pod
#
# The Secret is built from local files and never printed:
#   RCLONE_REMOTE     rclone remote holding the pCloud token (default pcloud-mcp; `rclone config create pcloud-mcp pcloud`)
#   PCLOUD_MCP_CONFIG folder with tunnel-token, webdav.env (RCLONE_PASS=...) and filter.txt
#                     (default ~/.config/pcloud-mcp, as in docs/setup.md)
set -euo pipefail
cd "$(dirname "$0")"
remote=${RCLONE_REMOTE:-pcloud-mcp}
dir=${PCLOUD_MCP_CONFIG:-$HOME/.config/pcloud-mcp}
k() { kubectl -n pcloud-mcp "$@"; }

for f in tunnel-token webdav.env filter.txt; do
  [ -s "$dir/$f" ] || { echo "missing $dir/$f" >&2; exit 1; }
done

kubectl apply -f namespace.yaml

if [ "${1:-}" = --rotate ]; then
  k delete secret pcloud-mcp-backend --ignore-not-found
fi
if ! k get secret pcloud-mcp-backend >/dev/null 2>&1; then
  # `rclone config dump` is JSON; the token is itself a JSON string inside it.
  dump=$(rclone config dump)
  field() { jq -er --arg r "$remote" --arg f "$1" '.[$r][$f]' <<<"$dump"; }
  [ "$(field type)" = pcloud ] || { echo "rclone remote '$remote' is not a pcloud remote" >&2; exit 1; }
  # `create`, not apply: apply would copy the values into an annotation.
  k create secret generic pcloud-mcp-backend \
    --from-file=pcloud-token=<(field token | tr -d '\n') \
    --from-file=pcloud-hostname=<(field hostname | tr -d '\n') \
    --from-file=webdav-password=<(sed -n 's/^RCLONE_PASS=//p' "$dir/webdav.env" | tr -d '\n') \
    --from-file=tunnel-token=<(tr -d '\n' <"$dir/tunnel-token") \
    --from-file=filter.txt="$dir/filter.txt"
  unset dump
  [ "${1:-}" = --rotate ] && k rollout restart deploy/pcloud-mcp-backend 2>/dev/null || true
fi

kubectl apply -f backend.yaml
k rollout status deploy/pcloud-mcp-backend --timeout=180s

# cloudflared logs one line per registered connection; expect four, all quic.
for _ in $(seq 1 15); do
  n=$(k logs deploy/pcloud-mcp-backend -c cloudflared 2>/dev/null | grep -c 'Registered tunnel connection' || true)
  [ "$n" -ge 4 ] && break
  sleep 2
done
echo "tunnel connections registered: $n"
k logs deploy/pcloud-mcp-backend -c rclone --tail=5
