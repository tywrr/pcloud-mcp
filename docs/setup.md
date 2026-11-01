# Setup

Running your own Chance of Files 0.3, step by step: an rclone remote for your pCloud, a Cloudflare Tunnel and Workers VPC service, the core's pod on your own Kubernetes or k3s node, the auth Worker, then the connector in Claude, and the index's first walk. Allow about an hour.

Chance of Files is an independent project, not affiliated with pCloud or Anthropic; neither supports it, so problems go to this repository's issues, not to them.

Upgrading from 0.2? See [From 0.2 to 0.3](#from-02-to-03). Upgrading from 0.1 (two Workers and a laptop)? See [Moving from 0.1](#moving-from-01) at the end. The 0.1 setup itself is at tag [`v0.1.0`](https://github.com/tywrr/chance-of-files/blob/v0.1.0/docs/setup.md).

What you end up with:

| Piece | Where | Name |
|---|---|---|
| rclone remote holding a pCloud token | Your workstation (copied into a Secret) | `chance-of-files` (your choice) |
| Cloudflare Tunnel, no public hostname | Cloudflare | `chance-of-files` |
| Workers VPC service → `127.0.0.1:8080` | Cloudflare | `chance-of-files` |
| Pod: core + rclone rcd + cloudflared | Your node, namespace `chance-of-files` | Deployment `chance-of-files` |
| Auth Worker, the connector URL | Cloudflare | `chance-of-files`, at `https://chance-of-files.<subdomain>.workers.dev` |
| KV namespace | Cloudflare | bound as `OAUTH_KV` |
| Custom connector | claude.ai | pCloud (your choice) |

And these files on your workstation, in `~/.config/chance-of-files/` (mode 700; `deploy/k8s/apply.sh` reads them and never prints them):

| File | Holds |
|---|---|
| `tunnel-token` | The tunnel's `eyJ…` token |
| `internal-token` | The secret shared by the auth Worker and the core; `apply.sh` makes it if missing |
| `rc-pass` | rclone's rc password, used only inside the pod; `apply.sh` makes it if missing |
| `auth.secrets` | The auth Worker's secrets, for its first deploy only |

## 1. Prerequisites

- **A node that runs containers**, always on, with about 1.5 GB of RAM free for walks of big folders (a few hundred MB at rest): a Kubernetes or k3s cluster you can reach with `kubectl` (a single-node k3s on a NAS or mini PC is the reference setup), or a plain Docker host ([Running on plain Docker](#running-on-plain-docker)). arm64 and amd64 both work.
- **A default StorageClass** for the 50 Gi PVC (k3s ships `local-path`), or a PersistentVolume of your own (step 5). The index needs well under 1 GB of it (see [the first walk](#11-the-index-first-walk)).
- **A Cloudflare account** on the free plan. Your `workers.dev` subdomain is shown under Workers & Pages.
- **On your workstation:** `kubectl`, `rclone` 1.75 or later, `jq`, `openssl`, `git`, and [mise](https://mise.jdx.dev) (`mise install` in a clone of this repository brings node and wrangler). To build the image yourself: Rust from [rustup](https://rustup.rs) and Python 3; `deploy/build-image.sh` gets cargo-zigbuild and zig through mise. No Docker is needed.

```bash
git clone https://github.com/tywrr/chance-of-files && cd chance-of-files
mise install
install -d -m 700 ~/.config/chance-of-files
```

## 2. Authorise rclone for pCloud

rclone uses its own registered pCloud app, so this works while pCloud's API doesn't hand out tokens to new apps. On a machine with a browser:

```bash
rclone config create chance-of-files pcloud
```

It opens pCloud's sign-in page; approve rclone. rclone picks the API host for your account's region (`api.pcloud.com` for US, `eapi.pcloud.com` for EU) and stores it with the token. Check it:

```bash
rclone about chance-of-files:
rclone lsf chance-of-files: --max-depth 1
```

Keep that remote: `deploy/k8s/apply.sh` copies its `token` and `hostname` into the cluster Secret. The token gives full access to your pCloud, so treat `~/.config/rclone/rclone.conf` like a password.

## 3. Create the tunnel

In the Cloudflare dashboard, open **Workers VPC > Tunnels** (the same tunnels appear under **Networking > Tunnels**) and create a tunnel named `chance-of-files`. Don't add a public hostname. Choose any connector type to see the install command, copy only the `eyJ…` token from it, and save it:

```bash
(umask 077 && cat > ~/.config/chance-of-files/tunnel-token)   # paste the token, then Ctrl-D
```

Note the tunnel's ID (shown on its page).

## 4. Create the VPC service

```bash
npx wrangler login
npx wrangler vpc service create chance-of-files --type http --tunnel-id <TUNNEL_ID> --ipv4 127.0.0.1 --http-port 8080
```

It prints the service's ID; `npx wrangler vpc service list` shows it again. The auth Worker reaches the core through this service, so `127.0.0.1:8080` is the address inside the pod where the core listens.

## 5. Configure the pod

`deploy/k8s/base` is a [kustomize](https://kustomize.io) base with generic defaults. Put your own settings in an **overlay**: a folder outside this repository (or in a private one) that refers to the base and replaces its config.

```text
my-overlay/
  kustomization.yaml
  config.toml
```

```yaml
# my-overlay/kustomization.yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
namespace: chance-of-files
resources:
  - https://github.com/tywrr/chance-of-files//deploy/k8s/base?ref=v0.3.0
images:
  - name: ghcr.io/tywrr/chance-of-files
    newName: localhost/chance-of-files     # the image you import in step 6
    newTag: "0.3.0"
configMapGenerator:
  - name: chance-of-files-config
    behavior: replace
    files: [config.toml]
```

Start `config.toml` from [`deploy/k8s/base/config.toml`](../deploy/k8s/base/config.toml); every option is explained in [`config.example.toml`](../config.example.toml). Keep `server.listen = "127.0.0.1:8080"` and the `[rclone]` section as they are: they match the pod. The part to think about is `[policy]`:

```toml
[policy]
# Never listed, searched or read. Absolute paths; each hides the folder (or file) and everything in it.
hidden = ["/Crypto Folder", "/Taxes", "/.Trash-1000"]
# Visible and readable, never written once write tools exist (0.4).
protected = ["/Backups/Immich"]
# search_files leaves these out of its results; Claude can still list, read and audit them.
search_skip = ["/Backups/Immich"]

[index]
# Where index.db lives: the data volume. Setting [server] state_dir = "/var/lib/chance-of-files"
# instead does the same (and later versions keep more there). With neither, the index is off.
dir = "/var/lib/chance-of-files/index"
# Walked on their own schedules; roots never overlap (a parent's walk goes around a child root).
roots = [
  { path = "/", every = "30m" },
  { path = "/Backups/Immich", every = "6h" },
]
```

- **Hide your Crypto Folder.** pCloud encrypts it client-side; there's no reason for Claude to see even its names. Hide anything else you wouldn't paste into a chat: IDs, tax papers, password exports.
- **`search_skip` is optional now.** Name search answers from the index, so it's fast over the whole drive; skip a photo backup only if its hundred thousand file names would crowd out what you're looking for.
- **Roots.** The default is one root, `/`, walked every 30 minutes. Give a huge tree that rarely changes, such as a photo backup, a root of its own with a longer `every`: each walk of `/` then goes around it, and it's walked on its own schedule.
- **A guide.** Write an `/AGENTS.md` at the root of your drive saying what each folder is for; its first 8 KiB go into the instructions Claude gets when it connects (`guide.path`).

**Storage.** The base asks the default StorageClass for 50 Gi, mounted at `/var/lib/chance-of-files`; 0.3 keeps `index/index.db` there (tens of MB for a drive of ~140,000 entries; `index_status` shows its size), and later releases add the mirror and upload staging. To use a directory you choose on the node, add a `local` PersistentVolume with a `nodeAffinity` for your node and a patch that sets the PVC's `storageClassName` and `volumeName` to it. Make the directory owned by `65532:65532`, the user every container runs as. `hostPath` volumes are refused by the namespace's `restricted` Pod Security profile.

## 6. Build the image

Until images are published on ghcr.io, build it yourself. `deploy/build-image.sh` cross-compiles a static musl binary with cargo-zigbuild, packs it into an OCI archive (no Docker, no registry), and can import it on a k3s node over ssh:

```bash
deploy/build-image.sh --import <node>            # builds for the node's architecture, then k3s ctr images import
deploy/build-image.sh --arch arm64               # or only build: target/oci/chance-of-files-<version>-arm64.tar
```

`--import` needs ssh access to the node and passwordless `sudo` there for `k3s ctr`. On another runtime, copy the archive over and load it with its own tool (`ctr -n k8s.io images import`, `docker load`, or push it to a registry with `skopeo copy oci-archive:…`). The image is `localhost/chance-of-files:<version>`, pulled with `IfNotPresent`, so the node must have it before the pod starts.

## 7. Deploy the pod

```bash
KUBECONFIG=~/.kube/<your-cluster>.yaml RCLONE_REMOTE=chance-of-files deploy/k8s/apply.sh my-overlay
```

It:

1. makes `internal-token` and `rc-pass` in `~/.config/chance-of-files` if they're missing;
2. applies the Namespace, then creates the Secret `chance-of-files` from those files, the tunnel token and the rclone remote (`create`, not `apply`, so the values don't end up in an annotation; nothing is printed);
3. applies the overlay, waits for the rollout, and waits for cloudflared to log **four** registered tunnel connections;
4. checks `GET /readyz` through `kubectl port-forward` (it's `200` once rclone answers pCloud).

Set `COF_EXPECT_NODE=<node name>` to make it refuse any other cluster, a guard against the wrong kubeconfig. Without an argument it applies the bare base (default config, image from ghcr.io).

Run **one connector per tunnel**: Cloudflare spreads requests across all of a tunnel's connectors, so make sure nothing else runs cloudflared with this token.

To check the core without Claude, forward its port and call it with the internal token:

```bash
kubectl -n chance-of-files port-forward deploy/chance-of-files 18080:8080 &
H=(-H "Authorization: Bearer $(cat ~/.config/chance-of-files/internal-token)" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')
curl -s "${H[@]}" http://127.0.0.1:18080/mcp -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq -r '.result.tools[].name'
kill %1
```

## 8. Deploy the auth Worker

The public `auth/wrangler.jsonc` has a `<SERVICE_ID>` placeholder. Make a personal copy with your VPC service's ID; it's gitignored, so it never ends up in a commit:

```bash
sed 's/<SERVICE_ID>/<the id from step 4>/' auth/wrangler.jsonc > auth/wrangler.personal.jsonc
```

The Worker's secrets: `INTERNAL_TOKEN` must equal the core's, and `OWNER_PASSPHRASE` is what you'll type when Claude connects.

```bash
(
  umask 077
  printf 'INTERNAL_TOKEN=%s\nOWNER_PASSPHRASE=%s\n' \
    "$(cat ~/.config/chance-of-files/internal-token)" "$(openssl rand -base64 24)" \
    > ~/.config/chance-of-files/auth.secrets
)
grep OWNER_PASSPHRASE ~/.config/chance-of-files/auth.secrets   # put this in your password manager
(cd auth && npm ci && npx wrangler deploy -c wrangler.personal.jsonc --secrets-file ~/.config/chance-of-files/auth.secrets)
```

Wrangler prints the URL, `https://chance-of-files.<subdomain>.workers.dev`. The first deploy creates the KV namespace `OAUTH_KV`. Later deploys reuse the secrets already set: `mise run deploy:auth`. Once it's up you can delete `auth.secrets` (the passphrase is in your password manager; the internal token is in `internal-token`). [auth/README.md](../auth/README.md) documents every setting of the Worker, including a custom domain.

Anyone who has the passphrase can connect their Claude to your files, so keep it long and random; attempts are rate limited per address, which slows one guesser but not one with many addresses.

Check the public side:

```bash
URL=https://chance-of-files.<subdomain>.workers.dev
curl -s $URL/healthz; echo                                          # ok
curl -si -X POST $URL/mcp | grep -i -e '^HTTP' -e www-authenticate  # 401, Bearer resource_metadata="…"
curl -s $URL/.well-known/oauth-protected-resource/mcp; echo         # "resource": "$URL/mcp"
```

## 9. Add the connector in Claude

On claude.ai (or in the desktop app):

1. Open **Customize > Connectors** and select **Add custom connector**.
2. Name: `pCloud`. URL: `https://chance-of-files.<subdomain>.workers.dev/mcp`, exactly, with no trailing slash.
3. Leave the OAuth client ID and secret empty. If the dialog asks how Claude identifies itself, choose **Use Claude's published identity** (CIMD).
4. Select **Add**, then **Connect**. Your Worker's sign-in page opens: enter the passphrase. Leave **Also allow changes** unticked: 0.3 has no write tools, and a read-only grant is the safer default. Approve; you land back in claude.ai and the connector shows **Connected**.

The connector's page should list `list_folder`, `get_file_info`, `read_text_file`, `read_image`, `search_files`, `folder_stats`, `query_files`, `find_duplicates`, `recent_changes`, `type_breakdown`, `index_status` and `refresh_index`, all read-only. Try it in a chat: **+ > Connectors**, turn on pCloud, and ask "What's in the top folder of my pCloud?".

For Claude Code:

```bash
claude mcp add --transport http pcloud https://chance-of-files.<subdomain>.workers.dev/mcp
```

then run `/mcp` in Claude Code to sign in.

## 10. Check it in Claude Duet

Duet uses the connectors on your claude.ai account; there's nothing to add. Ask it something like "Read notes/todo.md from pCloud and summarize it". Duet is a research preview: it uses custom connectors when you talk to it, but its scheduled and background runs may not.

## 11. The index: first walk

The core starts walking when it starts, one root at a time. Until a root's first walk has finished, index results say so in their header (`list_folder live=true` and `get_file_info` ask pCloud directly in the meantime). The listing part is quick, ~4 s for `/` around a photo backup and ~6 s for a 133k-file folder ([phase 0](design/phase0-rclone.md)); writing the entries into SQLite takes longer on the first walk than on later ones, which only apply the differences.

Ask Claude "What's the index status?", or call `index_status` yourself (as in step 7, with `"method":"tools/call","params":{"name":"index_status","arguments":{}}`). Per root you get when the last walk started and finished, how many entries it holds, the last error and any held removal; then the backend's health.

Then try the audit questions: "Which folders take the most space?", "What are my 20 largest files?", "Do I have duplicate videos over 100 MB?", "What changed in my drive this week?".

**When a walk would remove a lot.** If one walk of a root would remove more than `index.max_removal` (5%) of its entries, the core applies the walk's adds and changes but holds the removals, and `index_status` says so: "walk of / at 14:05Z would remove 7,412 entries (6.1%)". That protects the index from a truncated or flaky listing. If you did delete or move that much (check in pCloud's web UI), apply it: ask Claude to run `refresh_index` with `path` set to the root and `confirm=true`. If you didn't, leave it: the entries stay in the index, and `refresh_index` without `confirm` re-walks to check.

**Refreshing by hand.** `refresh_index` with a `path` re-walks the root that contains it and returns at once; `index_status` shows progress. Use it after big changes made outside Claude, instead of waiting for the next scheduled walk.

**Starting over.** The index is a cache. To rebuild it, stop the pod, delete `index/index.db` (and its `-wal` and `-shm` files) from the volume, and start the pod; the first walk rebuilds it.

## Running on plain Docker

The pod is three containers sharing one network namespace, which Docker does with `--network container:`. Load the image (`docker load -i target/oci/chance-of-files-<version>-<arch>.tar`), write a `config.toml` as in step 5, and put the secrets in an env file only root can read, for example `/etc/chance-of-files/env`:

```bash
INTERNAL_TOKEN=…
RC_PASS=…
RCLONE_RC_PASS=…                 # the same value as RC_PASS
RCLONE_CONFIG_PCLOUD_TOKEN=…     # rclone config dump | jq -r '."chance-of-files".token'
RCLONE_CONFIG_PCLOUD_HOSTNAME=api.pcloud.com
TUNNEL_TOKEN=eyJ…
```

```bash
docker run -d --name cof-rclone --restart unless-stopped --env-file /etc/chance-of-files/env \
  -e RCLONE_RC_USER=core -e RCLONE_CONFIG_PCLOUD_TYPE=pcloud -e RCLONE_CONFIG=/tmp/rclone.conf \
  rclone/rclone:1.75.2 rcd --rc-addr=127.0.0.1:5572 --rc-serve --tpslimit=8
docker run -d --name cof-core --restart unless-stopped --network container:cof-rclone --env-file /etc/chance-of-files/env \
  -v /etc/chance-of-files/config.toml:/etc/chance-of-files/config.toml:ro -v cof-data:/var/lib/chance-of-files \
  localhost/chance-of-files:0.3.0
docker run -d --name cof-cloudflared --restart unless-stopped --network container:cof-rclone --env-file /etc/chance-of-files/env \
  cloudflare/cloudflared:2026.10.2 tunnel --no-autoupdate --protocol quic run
```

Every container sees every secret this way; that's acceptable on a host only you use, and the Kubernetes setup gives each container only its own. Publish no ports: cloudflared dials out.

## Troubleshooting

Start with the pod:

```bash
kubectl -n chance-of-files get pods
kubectl -n chance-of-files logs deploy/chance-of-files -c core --tail=50
kubectl -n chance-of-files logs deploy/chance-of-files -c rclone --tail=50
kubectl -n chance-of-files logs deploy/chance-of-files -c cloudflared | grep -i -e registered -e error
```

### Connect loops back to sign-in, or every tool call fails with 401

- **The internal tokens differ.** The core answers `401` to every forwarded request, and Claude reads that as an expired token. Put the file's value on the Worker: `(cd auth && tr -d '\n' < ~/.config/chance-of-files/internal-token | npx wrangler secret put INTERNAL_TOKEN -c wrangler.personal.jsonc)`.
- **The URL in Claude isn't the protected resource.** It must equal `<public URL>/mcp` exactly. Check `curl -s $URL/.well-known/oauth-protected-resource/mcp`.

### Tools fail with "backend unavailable", or `/readyz` answers 503

The core can't reach pCloud through rclone. Look at rclone's log. A revoked or expired token: `rclone config reconnect chance-of-files:` on your workstation, then `deploy/k8s/apply.sh --rotate my-overlay`. A rate limit: wait; `--tpslimit=8` keeps normal use well under it.

### "Couldn't reach the MCP server", or a 502/504 from the Worker

The tunnel has no connector, or the VPC service points somewhere else. Check that cloudflared logged four registered connections, that `npx wrangler vpc service get <SERVICE_ID>` shows the right tunnel, `127.0.0.1` and port 8080, and that `service_id` in `auth/wrangler.personal.jsonc` matches.

### `apply.sh` reports fewer than four tunnel connections

cloudflared needs outbound UDP 7844 (QUIC). The NetworkPolicy allows it; a firewall in front of the node might not. Workers VPC doesn't work over http2, so the base pins `--protocol quic`.

### Index results look stale, or `index_status` shows an error

Each result header says when its root was last walked. If that's older than its `every` plus a few minutes, look at `index_status`: `last_error` is the walk's failure, and `backend unhealthy since …` means rclone or pCloud isn't answering (see the previous section). A held mass removal doesn't stop walks; it only holds removals until you confirm ([the first walk](#11-the-index-first-walk)). The core's log has the walks too: `kubectl -n chance-of-files logs deploy/chance-of-files -c core | grep -i walk`.

If the core restarts during a walk, look at `kubectl -n chance-of-files describe pod` for `OOMKilled`: raise the core's memory limit in your overlay (the base sets 2 Gi).

### A path Claude should see says "not found"

It's under a `policy.hidden` entry (hidden paths answer "not found" on purpose), or its case or Unicode form differs from what you typed: compare with `list_folder` on its parent. If it's new, the index may not have seen it yet: `list_folder` with `live=true` and `get_file_info` ask pCloud directly, and `refresh_index` re-walks.

### Sign-in succeeds, but the card stays on Connect or the tools never appear

Watch the Worker while you connect (`(cd auth && npx wrangler tail chance-of-files)`): after `/authorize` and `/token` you should see `POST /mcp` requests (`initialize`, `tools/list`) answered `Ok`. If they are, you've most likely hit a known claude.ai problem: [#831](https://github.com/anthropics/claude-ai-mcp/issues/831) (a stateless server's connector never leaves **Connect**) or [#476](https://github.com/anthropics/claude-ai-mcp/issues/476) (tools listed but not offered to the model in web chats). Workarounds people report: use the desktop or mobile app or Claude Code; **⋯ > Refresh tool list**; turn the connector on for the chat with **+ > Connectors**; or remove and re-add it.

## Updating

```bash
git pull
deploy/build-image.sh --import <node>         # the new version's image
# bump ref= and newTag in your overlay, then:
deploy/k8s/apply.sh my-overlay
mise run deploy:auth
```

### From 0.2 to 0.3

0.3 adds the index; nothing else changes in how you deploy it.

1. **Give the index a directory**, or it stays off. Your overlay replaces the base's whole `config.toml`, so the base's new `state_dir` doesn't reach you: add `state_dir = "/var/lib/chance-of-files"` under `[server]` (the data volume; the index then lives in `index/` there), or set `[index].dir`. With neither, 0.3 runs as 0.2 did, without the index or the audit tools, and logs a warning at startup. Then, optionally, an `[index]` section (step 5); its keys are `enabled` (`true`), `dir` (`<server.state_dir>/index`), `max_removal` (`0.05`), `hash_min_size` (`1048576`, bytes), `hash_in_background` (`true`) and `roots` (default: one root, `/`, every 30 minutes). `[rclone]` gains `health_every` (`5m`), how often rclone and pCloud are checked.
2. Reconsider `policy.search_skip`: name search now covers the whole drive from the index, so it's no longer needed for speed.
3. Bump `ref=v0.3.0` and `newTag: "0.3.0"`, build and import the image, and apply as above. The first walk starts with the pod; watch it with `index_status` ([step 11](#11-the-index-first-walk)).
4. Reconnect in Claude, or **⋯ > Refresh tool list** on the connector, so it picks up the new tools (Claude caches the tool list for a few minutes).

## Rotating secrets

| Secret | How |
|---|---|
| `OWNER_PASSPHRASE` | `(cd auth && npx wrangler secret put OWNER_PASSPHRASE -c wrangler.personal.jsonc)`. Connections already approved keep working until their refresh tokens expire or you delete their grants ([auth/README.md](../auth/README.md)). |
| Internal token | Write a new value to `~/.config/chance-of-files/internal-token` (`openssl rand -hex 32`), run `deploy/k8s/apply.sh --rotate my-overlay`, then put it on the Worker as under [Connect loops back to sign-in](#connect-loops-back-to-sign-in-or-every-tool-call-fails-with-401). Tool calls fail in between. |
| `rc-pass` | Delete the file and run `deploy/k8s/apply.sh --rotate my-overlay`; it never leaves the pod. |
| pCloud token | `rclone config reconnect chance-of-files:`, then `deploy/k8s/apply.sh --rotate my-overlay`. |
| Tunnel token | **Networking > Tunnels**, the tunnel, **Rotate token**; save the new `eyJ…` token to `tunnel-token`, then `deploy/k8s/apply.sh --rotate my-overlay`. The tunnel ID doesn't change, so the VPC service needs nothing. |

## Teardown

1. In Claude: **Customize > Connectors**, pCloud, **Remove**.
2. The pod: `kubectl delete namespace chance-of-files` (and any PersistentVolume you made; a `Retain` one keeps its directory on the node).
3. The Worker: `(cd auth && npx wrangler delete -c wrangler.personal.jsonc)`; its KV namespace: `npx wrangler kv namespace list`, then `npx wrangler kv namespace delete --namespace-id <ID>`.
4. The VPC service (`npx wrangler vpc service delete <SERVICE_ID>`) and the tunnel (**Workers VPC > Tunnels**).
5. Locally: `rm -r ~/.config/chance-of-files`, and `rclone config delete chance-of-files` if nothing else uses the remote.

## Moving from 0.1

0.1 ran two Workers (`chance-of-files` and `chance-of-files-core`) and rclone + cloudflared on a laptop or in a pod, on a tunnel and VPC service of their own. 0.2 and later are built to run **next to** it, so you can switch over without downtime:

1. Create a **new** tunnel and VPC service for 0.2 (steps 3 and 4); leave the old ones alone.
2. Deploy the pod (steps 5–7). If your 0.1 backend ran on Kubernetes, it's in the same namespace, `chance-of-files`, but under other names (`chance-of-files-backend`), so both run side by side, each on its own tunnel.
3. Deploy the 0.2 auth Worker (step 8). If your 0.1 auth Worker is also called `chance-of-files`, deploying replaces it in place: the connector URL stays and clients sign in again (its KV namespace is kept). Otherwise add the new URL in Claude next to the old connector.
4. Check the tools work, then remove the old connector.
5. Delete the 0.1 pieces: `chance-of-files-core` (`npx wrangler delete --name chance-of-files-core`), the old backend (`kubectl -n chance-of-files delete deploy/chance-of-files-backend secret/chance-of-files-backend networkpolicy/deny-ingress`, or the laptop's `systemd --user` units), the old VPC service and tunnel.

Run one cloudflared per tunnel throughout: the old and new tunnels are separate, so each keeps its own connector.
