# Network spike (v2 phase 0)

Measures what the v2 path can carry before the core is built on it ([docs/design/v2.md](../../docs/design/v2.md), phase 0 and "Per-call deadline"):

```
Claude / Duet / run.sh ─► https://chance-of-files-spike.<sub>.workers.dev/<SPIKE_KEY>/…
   ─► Worker chance-of-files-spike ─► Workers VPC service chance-of-files ─► tunnel chance-of-files
   ─► cloudflared (QUIC) ─► spike server 127.0.0.1:8080   (namespace chance-of-files-spike, cm3588)
```

It answers:

1. How long can a quiet response (plain HTTP, and an MCP tool call from Claude) stay open: 90, 150, 200 s? This sets `server.deadline` to the longest one that passes, minus 20 s.
2. Does a 2 GiB download arrive intact, how fast, and does `Range` resume work? Do three 20 GiB downloads in a row get through the free Worker?
3. Does a 2 GiB upload in 22 parts of 90 MiB get through the 100 MB body cap intact?
4. Can Duet's cloud computer reach the workers.dev host and download a verified file, and does its egress IP stay the same during a transfer (design D4)?

Nothing here touches pCloud, and nothing touches the live connector: namespace `pcloud-mcp`, tunnel `pcloud-mcp`, VPC service `pcloud-laptop` and Workers `pcloud-mcp*` stay as they are. The spike uses a separate Worker (`chance-of-files-spike`) and namespace. The tunnel and VPC service are the ones v2 keeps (migration step 1), so they stay after the spike.

## What's here

| File | What |
|---|---|
| `src/` | The spike server (axum). `GET /sleep?s=N` (N ≤ 300), `GET /blob?bytes=N` (deterministic bytes, N ≤ 25 GiB, `Range`), `GET /blob/sha256?bytes=N`, `PUT /chunk?upload=ID&part=K` (≤ 95 MiB, hashed while streaming, nothing stored), `POST /finish?upload=ID`, `POST /mcp` (stateless MCP, tools `hold` and `blob_url`). Every request needs `X-Spike-Token` = `SPIKE_TOKEN`. |
| `worker/` | Worker `chance-of-files-spike`. It forwards only requests under `/<SPIKE_KEY>/…` (or with `?k=<SPIKE_KEY>`) and adds `X-Spike-Token`. Everything else gets a 404 and never reaches the tunnel. Bodies stream both ways. |
| `k8s.yaml` | Namespace `chance-of-files-spike` (restricted), a single-replica Deployment (spike + cloudflared 2026.10.2 `--protocol quic`), and a NetworkPolicy that denies ingress. |
| `Dockerfile` | A debian-slim image, multi-arch with `docker buildx`. |
| `oci-image.py` | Packs a static binary into an OCI archive without Docker, for `k3s ctr images import`. |
| `run.sh` | Client-side measurements with a results table. |

The spike is its own Cargo project. The root `Cargo.toml` excludes `spikes/`, so `cargo test --workspace` doesn't build it. Run `cargo test` in this directory to test it.

## 1. Secrets

```sh
SPIKE_TOKEN=$(openssl rand -hex 32)   # Worker -> server
SPIKE_KEY=$(openssl rand -hex 16)     # in the public URL; anyone with the URL can use the spike
```

Keep both until the spike is deleted. `SPIKE_KEY` is in every URL, Claude connector and Duet transcript, so treat it as low-value and delete the Worker afterwards.

## 2. Image (arm64 for the cm3588)

Build hosts checked on 2026-10-31:

- **Laptop:** Docker with buildx 0.38.1 is installed and QEMU `binfmt` for aarch64 is registered. But `docker.service` is inactive, and `ty` isn't in the `docker` group, so it can't be used without sudo. No podman or cross.
- **Laptop:** `cargo-zigbuild` 0.24.0 and zig 0.14.0 are installed through mise (not activated globally), and the `aarch64-unknown-linux-musl` target is installed.
- **cm3588:** it has `k3s` and `ctr` but no `cargo`, `podman`, `docker` or `nerdctl`, so it can't build. It can import an image archive.

**Path A (no Docker, verified):** cross-compile a static binary, pack it, and import it on the node.

```sh
cd spikes/network
mise x cargo:cargo-zigbuild@0.24.0 zig@0.14.0 -- cargo zigbuild --release --target aarch64-unknown-linux-musl
BIN=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')/aarch64-unknown-linux-musl/release/cof-network-spike
python3 oci-image.py "$BIN" arm64 localhost/chance-of-files-spike:0.1.0 spike-arm64.tar
scp spike-arm64.tar cm3588:/tmp/
ssh cm3588 'sudo k3s ctr -n k8s.io images import /tmp/spike-arm64.tar && sudo k3s ctr -n k8s.io images ls | grep chance-of-files-spike && rm /tmp/spike-arm64.tar'
```

The binary is about 1.5 MB, statically linked (musl). It was checked under qemu-aarch64: it serves the same blob hashes as the x86 build. The image contains only the binary (plus `/tmp`), not debian-slim; the server needs nothing else. Image reference: `localhost/chance-of-files-spike:0.1.0`, with `imagePullPolicy: IfNotPresent`.

**Path B (the Dockerfile, debian-slim):** use this where Docker is usable: after `sudo systemctl start docker`, as root, or in CI.

```sh
cd spikes/network
sudo docker buildx build --platform linux/arm64 -t localhost/chance-of-files-spike:0.1.0 \
  -o type=docker,dest=spike-arm64.tar .        # then scp + k3s ctr import as in path A
# or both architectures to a registry (needs a registry login):
docker buildx build --platform linux/arm64,linux/amd64 -t ghcr.io/<owner>/chance-of-files-spike:0.1.0 --push .
```

A Kaniko Job in the cluster would also work, but it needs a registry to push to. Path A needs no registry.

## 3. Workers VPC service, Worker

The owner creates tunnel `chance-of-files` in the dashboard (Workers VPC → Tunnels) with no public hostname, and keeps its `eyJ…` token. Then (needs `wrangler login`):

```sh
cd spikes/network/worker
npm ci
npx wrangler vpc service create chance-of-files --type http --tunnel-id <TUNNEL_ID> --ipv4 127.0.0.1 --http-port 8080
# Keep the service ID out of the public repo: a gitignored copy of the config holds it.
sed 's/<SERVICE_ID>/<the service_id printed above>/' wrangler.jsonc > wrangler.personal.jsonc
npx wrangler deploy -c wrangler.personal.jsonc \
  --secrets-file <(printf 'SPIKE_TOKEN=%s\nSPIKE_KEY=%s\n' "$SPIKE_TOKEN" "$SPIKE_KEY")
```

Wrangler prints the URL: `https://chance-of-files-spike.<sub>.workers.dev`. The spike's base URL is that origin plus `/$SPIKE_KEY`.

## 4. Cluster

```sh
sed 's|<SPIKE_IMAGE>|localhost/chance-of-files-spike:0.1.0|' spikes/network/k8s.yaml | kubectl apply -f -
kubectl -n chance-of-files-spike create secret generic chance-of-files-spike \
  --from-literal=spike-token="$SPIKE_TOKEN" --from-literal=tunnel-token='<eyJ… tunnel token>'
kubectl -n chance-of-files-spike rollout status deploy/network-spike
kubectl -n chance-of-files-spike logs deploy/network-spike -c cloudflared | grep -i 'registered tunnel connection'
```

Before and after, run the design's one-connector-per-tunnel check (docs/design/v2.md, Migration):

```sh
kubectl get pods -A -o jsonpath='{range .items[*]}{.metadata.namespace}{"\t"}{.metadata.name}{"\t"}{.status.phase}{"\t"}{.spec.containers[*].image}{"\n"}{end}' | grep cloudflared
```

Expect one cloudflared in `pcloud-mcp`, unchanged, and one in `chance-of-files-spike`. The dashboard should show one connector on tunnel `chance-of-files`. When v2's own pod (namespace `chance-of-files`) comes in phase 1, delete the spike namespace first: both use the same tunnel.

## 5. Measurements

From any machine (results print as a table; the exit code is non-zero if any check fails):

```sh
BASE=https://chance-of-files-spike.<sub>.workers.dev/$SPIKE_KEY
spikes/network/run.sh "$BASE"          # MCP smoke, sleep 90/150/200, 2 GiB download, resume, 22 x 90 MiB upload
spikes/network/run.sh "$BASE" --big    # the same plus 3 x 20 GiB downloads (hours on a home uplink)
```

If something fails, rule out the pod by bypassing Cloudflare:

```sh
kubectl -n chance-of-files-spike port-forward deploy/network-spike 18080:8080 &
SPIKE_TOKEN=$SPIKE_TOKEN spikes/network/run.sh http://127.0.0.1:18080 --quick
```

`--quick` uses tiny sizes and 2–3 s sleeps, to check the script itself. The server logs one line per request with the client IP the Worker saw: `kubectl -n chance-of-files-spike logs deploy/network-spike -c spike`.

## 6. Claude and Duet (the owner)

**Claude: tool calls held open.** In claude.ai, open Settings → Connectors → Add custom connector. Use the URL `https://chance-of-files-spike.<sub>.workers.dev/<SPIKE_KEY>/mcp`, with no OAuth client ID or secret ("No sign-in"; the Worker 404s OAuth discovery). Then, in a new chat with only this connector on:

- "Call the hold tool with seconds 90." Then 150, then 200, each in a new message.
- For each one, record whether "held N s" came back, or what error appeared and after how long (use a stopwatch, or the timestamps in the server log).
- Set `server.deadline` in v2 to the longest that passed, minus 20 s. If 90 s fails, retry with 60 s.

**Duet: download through the allowlist.**

- Add the host `chance-of-files-spike.<sub>.workers.dev` to Duet's network allowlist. Use the host only, with no key or path.
- Make sure the connector is available to Duet. Ask: "Call blob_url with bytes 2147483648. On your computer, download the URL with `curl -fSL -C - -o blob.bin '<url>'`, check `sha256sum blob.bin` against the sha256 it gave, and tell me the time and speed."
- Optional resume check: "Interrupt the download partway (e.g. `timeout 20 curl …`), then rerun the same `curl -C -` command and verify again."
- Egress stability: during and after, check `kubectl … logs -c spike` to see whether every `/blob` request from Duet came from one IP or one /24. Record this for D4.

Record the results in the phase 0 notes: `run.sh` tables, Claude hold results, Duet's time, speed, sha and IPs.

## 7. Clean up

```sh
kubectl delete namespace chance-of-files-spike
cd spikes/network/worker && npx wrangler delete -c wrangler.personal.jsonc
ssh cm3588 'sudo k3s ctr -n k8s.io images rm localhost/chance-of-files-spike:0.1.0'
```

Also remove the spike connector in claude.ai and the host from Duet's allowlist. Keep tunnel `chance-of-files` and VPC service `chance-of-files`: v2 uses them.
