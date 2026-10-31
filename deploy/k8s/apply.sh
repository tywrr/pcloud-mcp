#!/usr/bin/env bash
# Deploys Chance of Files to the cluster in $KUBECONFIG: the Secret from local files, then the
# kustomize base (or your overlay), then waits for the rollout, the tunnel and /readyz.
#
#   deploy/k8s/apply.sh [DIR]            create the Secret if missing, apply DIR (default: the base)
#   deploy/k8s/apply.sh --rotate [DIR]   replace the Secret from the files below, then restart the pod
#
# DIR is any kustomization (a directory with kustomization.yaml) that builds on deploy/k8s/base.
#
# The Secret chance-of-files is built from local files and never printed:
#   $COF_SECRETS_DIR (default ~/.config/chance-of-files), mode 700:
#     tunnel-token     the tunnel's eyJ... token (required)
#     internal-token   the auth Worker's INTERNAL_TOKEN; made here if missing (set the same value
#                      on the Worker: docs/setup.md)
#     rc-pass          rclone's rc password, used only inside the pod; made here if missing
#   $RCLONE_REMOTE (default chance-of-files): an authorised rclone pcloud remote on this machine
#     (`rclone config create chance-of-files pcloud`); its token and API host go into the Secret.
#
# $COF_EXPECT_NODE, if set, makes the script refuse unless the cluster's only node has that name
# (a guard against the wrong kubeconfig). $COF_LOCAL_PORT (default 18080) is the local port of the
# /readyz check.
set -euo pipefail

rotate=0
if [ "${1:-}" = --rotate ]; then rotate=1; shift; fi
dir=${1:-"$(dirname "$0")/base"}
[ -f "$dir/kustomization.yaml" ] || { echo "no kustomization.yaml in $dir" >&2; exit 1; }
secrets=${COF_SECRETS_DIR:-$HOME/.config/chance-of-files}
remote=${RCLONE_REMOTE:-chance-of-files}
ns=chance-of-files
k() { kubectl -n "$ns" "$@"; }

if [ -n "${COF_EXPECT_NODE:-}" ]; then
  node=$(kubectl get nodes -o jsonpath='{.items[*].metadata.name}')
  [ "$node" = "$COF_EXPECT_NODE" ] || { echo "refusing: cluster node is '$node', not $COF_EXPECT_NODE" >&2; exit 1; }
fi

[ -s "$secrets/tunnel-token" ] || { echo "missing $secrets/tunnel-token" >&2; exit 1; }
for f in internal-token rc-pass; do
  if [ ! -s "$secrets/$f" ]; then
    (umask 077 && openssl rand -hex 32 | tr -d '\n' >"$secrets/$f")
    echo "made $secrets/$f"
  fi
done

# The Namespace first and alone (its manifest carries the selected label), so the Secret can exist
# before the pod does.
kubectl apply -k "$dir" -l kubernetes.io/metadata.name="$ns"

created=0
if [ "$rotate" = 1 ]; then
  k delete secret chance-of-files --ignore-not-found
fi
if ! k get secret chance-of-files >/dev/null 2>&1; then
  # `rclone config dump` is JSON; the token is itself a JSON string inside it.
  dump=$(rclone config dump)
  field() { jq -er --arg r "$remote" --arg f "$1" '.[$r][$f]' <<<"$dump"; }
  [ "$(field type)" = pcloud ] || { echo "rclone remote '$remote' is not a pcloud remote" >&2; exit 1; }
  # Read into variables so a missing field stops the script instead of creating an empty key.
  tok=$(field token | tr -d '\n'); host=$(field hostname | tr -d '\n')
  if [ -z "$tok" ] || [ -z "$host" ]; then echo "rclone remote '$remote' has no token or hostname" >&2; exit 1; fi
  # `create`, not apply: apply would copy the values into an annotation.
  k create secret generic chance-of-files \
    --from-file=internal-token=<(tr -d '\n' <"$secrets/internal-token") \
    --from-file=rc-pass=<(tr -d '\n' <"$secrets/rc-pass") \
    --from-file=pcloud-token=<(printf %s "$tok") \
    --from-file=pcloud-hostname=<(printf %s "$host") \
    --from-file=tunnel-token=<(tr -d '\n' <"$secrets/tunnel-token")
  unset dump
  created=1
fi

kubectl apply -k "$dir"
# A pod that started before a new Secret existed keeps the old env until it restarts.
if [ "$rotate" = 1 ] && [ "$created" = 1 ]; then
  k rollout restart deploy/chance-of-files
fi
k rollout status deploy/chance-of-files --timeout=300s

# cloudflared logs one line per registered connection; expect four.
n=0
for _ in $(seq 1 30); do
  n=$(k logs deploy/chance-of-files -c cloudflared 2>/dev/null | grep -c 'Registered tunnel connection' || true)
  [ "$n" -ge 4 ] && break
  sleep 2
done
echo "tunnel connections registered: $n"

# The core listens on the pod's loopback only; port-forward connects there.
port=${COF_LOCAL_PORT:-18080}
k port-forward deploy/chance-of-files "$port:8080" >/dev/null 2>&1 &
pf=$!
trap 'kill "$pf" 2>/dev/null || true' EXIT
ready=""
for _ in $(seq 1 15); do
  ready=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/readyz" || true)
  [ "$ready" = 200 ] && break
  sleep 2
done
echo "readyz: ${ready:-no answer}"
if [ "$n" -lt 4 ] || [ "$ready" != 200 ]; then k logs deploy/chance-of-files -c core --tail=20 >&2; exit 1; fi
