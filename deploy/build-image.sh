#!/usr/bin/env bash
# Builds the chance-of-files image without Docker: a static musl binary cross-compiled with
# cargo-zigbuild, packed into an OCI archive by deploy/oci-image.py, optionally imported on a k3s
# node over ssh.
#
#   deploy/build-image.sh [--arch arm64|amd64|all] [--tag TAG] [--import SSH_HOST]
#
#   --arch    what to build (default: all). With --import, default: the node's own architecture
#   --tag     image tag (default: the server crate's version from Cargo.toml)
#   --import  scp the archive to SSH_HOST and `sudo k3s ctr -n k8s.io images import` it there
#
# $IMAGE (default localhost/chance-of-files) names the image; archives land in target/oci/.
# Needs rustup, mise (for cargo-zigbuild 0.24.0 and zig 0.14.0) and python3. --import needs
# passwordless sudo on the node.
set -euo pipefail
cd "$(dirname "$0")/.."

arch="" tag="" host=""
while [ $# -gt 0 ]; do
  case $1 in
    --arch) arch=$2; shift 2 ;;
    --tag) tag=$2; shift 2 ;;
    --import) host=$2; shift 2 ;;
    -h | --help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
done
image=${IMAGE:-localhost/chance-of-files}
meta=$(cargo metadata --format-version 1 --no-deps)

if [ -z "$tag" ]; then
  # The package that builds the chance-of-files binary, whatever the package is called.
  tag=$(python3 -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    if any(t["name"] == "chance-of-files" and "bin" in t["kind"] for t in p["targets"]):
        print(p["version"])
        break
' <<<"$meta")
  [ -n "$tag" ] || { echo "no chance-of-files binary in the workspace" >&2; exit 1; }
fi

if [ -z "$arch" ]; then
  if [ -n "$host" ]; then
    case $(ssh "$host" uname -m) in
      aarch64 | arm64) arch=arm64 ;;
      x86_64) arch=amd64 ;;
      *) echo "unknown architecture on $host" >&2; exit 1 ;;
    esac
  else
    arch=all
  fi
fi
case $arch in
  all) arches=(arm64 amd64) ;;
  arm64 | amd64) arches=("$arch") ;;
  *) echo "--arch must be arm64, amd64 or all" >&2; exit 2 ;;
esac

target_dir=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' <<<"$meta")
mkdir -p "$target_dir/oci"
for a in "${arches[@]}"; do
  case $a in
    arm64) triple=aarch64-unknown-linux-musl ;;
    amd64) triple=x86_64-unknown-linux-musl ;;
  esac
  rustup target add "$triple" >/dev/null
  mise x cargo:cargo-zigbuild@0.24.0 zig@0.14.0 -- \
    cargo zigbuild --release --locked --target "$triple" --bin chance-of-files
  out="$target_dir/oci/chance-of-files-$tag-$a.tar"
  python3 deploy/oci-image.py "$target_dir/$triple/release/chance-of-files" "$a" "$image:$tag" "$out"
done

if [ -n "$host" ]; then
  [ ${#arches[@]} = 1 ] || { echo "--import takes one architecture" >&2; exit 2; }
  remote_tar=/tmp/chance-of-files-$tag-${arches[0]}.tar
  scp -q "$out" "$host:$remote_tar"
  # shellcheck disable=SC2029 # expanded here on purpose: the names are local
  ssh "$host" "sudo k3s ctr -n k8s.io images import '$remote_tar' && rm -f '$remote_tar' && sudo k3s ctr -n k8s.io images ls -q | grep -F '$image:$tag'"
fi
