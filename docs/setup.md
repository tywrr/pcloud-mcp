# Setup

Running your own Chance of Files, step by step: the laptop side (rclone and cloudflared as `systemd --user` services), the Cloudflare side (a tunnel, a VPC service, two Workers), then the connector in Claude. Written for Arch Linux; other systemd distributions differ only in package names. Allow about an hour.

Chance of Files is an independent project, not affiliated with pCloud or Anthropic; neither supports it, so problems go to this repository's issues, not to them.

What you end up with:

| Piece | Where | Name |
|---|---|---|
| WebDAV server over one pCloud Drive folder, or all of it | Laptop, `systemd --user` | `chance-of-files-webdav.service` |
| Tunnel connector | Laptop, `systemd --user` | `chance-of-files-tunnel.service` |
| Cloudflare Tunnel, no public hostname | Cloudflare | `pcloud-laptop` (your choice) |
| Workers VPC service | Cloudflare | `pcloud-laptop` (your choice) |
| Core Worker, no public URL | Cloudflare | `chance-of-files-core` |
| Auth Worker, the connector URL | Cloudflare | `chance-of-files`, at `https://chance-of-files.<subdomain>.workers.dev` |
| KV namespace | Cloudflare | bound as `OAUTH_KV` |
| Custom connector | claude.ai | pCloud (your choice) |

And these files on the laptop, all in `~/.config/chance-of-files/` (mode 700):

| File | Holds |
|---|---|
| `webdav.env` | rclone's Basic auth user and password, and the folder to serve |
| `rclone-filter.txt` | Optional: what rclone hides when it serves the whole drive |
| `tunnel-token` | The tunnel token |
| `core.secrets` | The core Worker's secrets, for its first deploy |
| `auth.secrets` | The auth Worker's secrets, for its first deploy |

## 1. Prerequisites

You need:

- A **Cloudflare account**. The free plan works for a focused folder; [step 2](#2-choose-the-folder) has the numbers. Your `workers.dev` subdomain is shown under Workers & Pages in the dashboard; pick one there if you haven't yet.
- **pCloud Drive**, installed, signed in and mounted at `~/pCloudDrive`.
- A **Claude** plan with custom connectors: Free allows one; Pro or Max for Claude Duet.

Install the tools (cloudflared is in Arch's `extra` repository):

```bash
sudo pacman -S --needed git openssl rustup rclone cloudflared
rustup default stable
rustup target add wasm32-unknown-unknown
cargo install worker-build@0.8.7 --locked
cloudflared --version        # 2025.7.0 or later: Workers VPC needs it (--token-file needs 2025.4.0)
```

Node and wrangler: the repository pins Node 26 and wrangler 4.156.0 in `mise.toml`, so with [mise](https://mise.jdx.dev) it's one command. Without mise, install Node 26 (`sudo pacman -S nodejs npm`) and `npm install -g wrangler@4.156.0`.

```bash
git clone https://github.com/tywrr/chance-of-files
cd chance-of-files
mise install                 # node 26 and wrangler 4.156.0
wrangler login               # opens the browser; wrangler's OAuth login
```

Every command below runs from the repository root; the ones for a subfolder `cd` into it in a subshell.

## 2. Choose the folder

Pick **one folder** under `~/pCloudDrive` for Claude to read. Everything in it, at any depth, is readable through the connector, so:

- **Not the Crypto Folder.** It's unreadable while locked, so rclone gets "permission denied", and it's the wrong place for this anyway.
- **Not a folder with secrets**: IDs, tax papers, bank statements, password exports.
- **Not the whole drive** unless you really mean it. If you do, read [Serving the whole drive](#serving-the-whole-drive) first: it hides the Crypto Folder and keeps search working around huge folders.
- **Small enough for the free plan.** A Worker on the free plan gets 10 ms of CPU per request, and parsing listings costs CPU. `search_files` stays under it up to roughly 1,000 files and folders under the path it searches (the whole folder by default), and `list_folder` up to roughly 1,000 to 1,500 entries in one folder; `read_image` takes images up to 1.5 MiB. A listing over `MAX_LISTING_BYTES` (400,000 bytes) fails cleanly, with a message telling Claude to narrow the path, so a bigger folder still works for the parts Claude can narrow down to. On Workers Paid ($5 a month, 30 s of CPU per request) you can serve more and raise `MAX_LISTING_BYTES` and `MAX_IMAGE_BYTES` ([configuration reference](#configuration-reference)).

The examples below use `~/pCloudDrive/Library`. Spaces in the name are fine.

### Serving the whole drive

You can serve `~/pCloudDrive` itself instead of one folder. Then **everything in your pCloud is readable by anyone who has the passphrase, through Claude**, except what you hide with the filter below. Think about the IDs, tax papers and password exports you keep there before you do it.

A bonus: with the drive root served, paths here match pCloud's own (`/Documents/lease.pdf` is `/Documents/lease.pdf` in both), so pCloud's [official connector](https://mcp.pcloud.com/mcp) pairs well with this one. It searches the whole drive on pCloud's side and manages files; this one reads what's inside them. Claude can find a file with one and read it with the other.

It takes four changes on top of the steps below.

**1. Point `CHANCE_OF_FILES_DIR` at the drive** when you write `webdav.env` in step 3: `CHANCE_OF_FILES_DIR=$HOME/pCloudDrive` in that snippet.

**2. Hide what Claude shouldn't see** with an rclone filter file. [`deploy/rclone-filter.txt`](../deploy/rclone-filter.txt) hides the Crypto Folder (pCloud Drive shows its files in plain text whenever it's unlocked), desktop trash folders, `.DS_Store` and `.directory` files, and pCloud Drive's `/.pcloudicon.png`. Add your own rules to it; the comments in the file explain the syntax. rclone takes any of its flags from an `RCLONE_` environment variable, so `RCLONE_FILTER_FROM` in `webdav.env` is the `--filter-from` option, with no change to the unit. Once step 3 is done:

```bash
cp deploy/rclone-filter.txt ~/.config/chance-of-files/rclone-filter.txt
echo "RCLONE_FILTER_FROM=$HOME/.config/chance-of-files/rclone-filter.txt" >> ~/.config/chance-of-files/webdav.env
systemctl --user restart chance-of-files-webdav
```

The path must be absolute and the file must exist, or rclone won't start (it reads the variable as a comma-separated list, so the path can't contain a comma). Check that the Crypto Folder is gone from the listing:

```bash
curl -s -u "mcp:$(sed -n 's/^RCLONE_PASS=//p' ~/.config/chance-of-files/webdav.env)" \
  -X PROPFIND -H 'Depth: 1' http://127.0.0.1:8080/ | grep -o '<D:href>[^<]*</D:href>'
```

Hidden files are gone from listings, searches and reads alike: rclone answers `404` for them.

**3. Turn search off in huge folders** with `SEARCH_SKIP`. `search_files` reads the whole tree under the path it searches in one WebDAV listing, and refuses when that's over `MAX_LISTING_BYTES` (about 1,000 entries; see step 2's numbers). A photo backup of 130,000 files means every search from `/` is refused. To find the big ones, count the entries in each top-level folder (this walks the mount, so give it a while):

```bash
for d in ~/pCloudDrive/*/; do printf '%8d  %s\n' "$(find "$d" | wc -l)" "$d"; done
```

List the folders search should stay out of, comma-separated, as paths from the root of what you serve. Set them as a secret, which keeps your folder names out of the public `wrangler.toml`:

```bash
(cd crates/worker && printf %s '/Backups/Immich' | wrangler secret put SEARCH_SKIP)
```

(A var in `crates/worker/wrangler.toml` works too; use one or the other, not both.) A secret takes effect at once; a var with the next deploy. Then:

- A search at or inside a turned-off folder is refused with a message telling Claude to browse it with `list_folder` instead. `list_folder` still shows up to 1,000 entries per folder.
- A search above one (from `/`, say) goes around it: it lists the folders on the way down to it, matches their names itself, and searches each other subfolder on its own. A subfolder that's still too big is left out with a note naming it, so Claude can search inside it with a narrower path; the rest of the results still come back. One search makes at most 40 of these lookups, and all of them together parse at most `MAX_LISTING_BYTES` and receive at most twice that (an answer too big to use still counts for what was read of it), so it stays within the free plan's CPU and subrequest limits.
- The folder's own name still matches: searching for "immich" finds `/Backups/Immich`.

**4. Write a guide for Claude.** If there's a file named `AGENTS.md` at the root of what you serve (`~/pCloudDrive/AGENTS.md`, so `/AGENTS.md` in any pCloud client), its first 8 KiB go into the instructions Claude gets when it connects. Say what each folder is for and where things live, so Claude doesn't have to explore to find out:

```markdown
# My pCloud

- Documents/: scanned letters and contracts, by year
- Projects/: one folder per project; each has a README.md
- Backups/: machine backups. Backups/Immich is the photo library (130,000 files); browse it by year and month.
```

Edit it from anywhere, the web or the phone app included. Claude reads it again each time it connects to the server, so a change shows up in the next conversation (clients that cache the server's details may take up to an hour). A longer guide is cut at 8 KiB, with a note telling Claude to read the rest with `read_text_file`. `GUIDE_PATH` names another file; set it to the empty string (`GUIDE_PATH = ""` in `wrangler.toml`) to turn the guide off. The guide counts as the owner's word, so anyone who can change that file in your pCloud can steer what Claude does with the connector.

## 3. Serve it with rclone

Create the config folder and the rclone settings. The password is random; you never type it, it only has to match the core Worker's `BACKEND_PASSWORD` later.

```bash
mkdir -p ~/.config/chance-of-files && chmod 700 ~/.config/chance-of-files
cat > ~/.config/chance-of-files/webdav.env <<EOF
RCLONE_USER=mcp
RCLONE_PASS=$(openssl rand -hex 24)
CHANCE_OF_FILES_DIR=$HOME/pCloudDrive/Library
EOF
chmod 600 ~/.config/chance-of-files/webdav.env
```

`CHANCE_OF_FILES_DIR` must be an absolute path (systemd doesn't expand `~` in this file); write it without quotes even if it has spaces.

Install both user services (the tunnel one is started in step 5) and start rclone:

```bash
mkdir -p ~/.config/systemd/user
cp deploy/systemd/chance-of-files-webdav.service deploy/systemd/chance-of-files-tunnel.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now chance-of-files-webdav
systemctl --user status chance-of-files-webdav
```

The unit runs pacman's `/usr/bin/rclone` from step 1, which is the simplest choice. If you use mise's rclone instead (`mise use -g rclone`), point the unit at mise's shim, which keeps working across upgrades, with `systemctl --user edit chance-of-files-webdav`:

```ini
[Service]
ExecStart=
ExecStart=%h/.local/share/mise/shims/rclone serve webdav ${CHANCE_OF_FILES_DIR} --addr 127.0.0.1:8080 --read-only --dir-cache-time 1m
```

Then `systemctl --user restart chance-of-files-webdav`. The path `mise which rclone` prints works too, but it has the version in it, so redo the override after every upgrade.

If pCloud Drive isn't running, the folder doesn't exist yet; the service fails its start check and retries every 30 seconds until it does.

Check it. The development backend (`mise run backend`) listens on 8081, so it can run alongside, but make sure the service is what answers on 8080: it should be `active`, with one `rclone` listening on `127.0.0.1:8080`.

```bash
systemctl --user is-active chance-of-files-webdav
ss -ltnp 'sport = :8080'
```

Without a password you get `401`; with it, an XML listing of the folder:

```bash
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8080/
curl -s -u "mcp:$(sed -n 's/^RCLONE_PASS=//p' ~/.config/chance-of-files/webdav.env)" \
  -X PROPFIND -H 'Depth: 1' http://127.0.0.1:8080/ | head -c 600; echo
```

## 4. Create the tunnel

Wrangler's OAuth login can't create tunnels, so use the dashboard:

1. Open **Workers VPC > Tunnels** (<https://dash.cloudflare.com/?to=/:account/workers/vpc/tunnels>) and select **Create**.
2. Name it `pcloud-laptop` and select **Save tunnel**.
3. The next page shows install commands for your OS. **Don't run them**: they install cloudflared as a root system service with the token in its unit file. Copy only the token: the long string that starts with `eyJ`.
4. **Don't add a public hostname** or any published route. The tunnel is reached only through Workers VPC.
5. Note the tunnel's **ID** (a UUID); step 6 needs it.

Save the token without it landing in your shell history:

```bash
read -rsp 'Tunnel token: ' TOKEN && printf '%s' "$TOKEN" > ~/.config/chance-of-files/tunnel-token && unset TOKEN; echo
chmod 600 ~/.config/chance-of-files/tunnel-token
```

(With an API token that has **Account > Cloudflare Tunnel > Edit**, you can create the tunnel through the API instead; the dashboard is simpler for a one-off.)

## 5. Run cloudflared

```bash
systemctl --user enable --now chance-of-files-tunnel
journalctl --user -u chance-of-files-tunnel -n 30 --no-pager
```

You should see four `Registered tunnel connection` lines with `protocol=quic`, and the tunnel turns **Healthy** in the dashboard. A warning about UDP buffer sizes is harmless.

Both services start when you log in. `loginctl enable-linger` wouldn't help here: pCloud Drive needs your desktop session anyway.

## 6. Create the VPC service

This points a Workers VPC service at rclone's address, as seen from cloudflared:

```bash
wrangler vpc service create pcloud-laptop --type http --tunnel-id <TUNNEL_ID> --ipv4 127.0.0.1 --http-port 8080
```

It prints the service's **ID**; `wrangler vpc service list` shows it again later. Workers VPC is in beta and free on all plans. Creating services needs the Connectivity Directory Admin role, which account Super Administrators have.

## 7. Generate the Worker secrets

The two Workers share `INTERNAL_TOKEN`; the core's `BACKEND_PASSWORD` is rclone's `RCLONE_PASS`; `OWNER_PASSPHRASE` is what you'll type when Claude connects.

```bash
INTERNAL_TOKEN=$(openssl rand -hex 32)
cat > ~/.config/chance-of-files/core.secrets <<EOF
INTERNAL_TOKEN=$INTERNAL_TOKEN
BACKEND_PASSWORD=$(sed -n 's/^RCLONE_PASS=//p' ~/.config/chance-of-files/webdav.env)
EOF
cat > ~/.config/chance-of-files/auth.secrets <<EOF
INTERNAL_TOKEN=$INTERNAL_TOKEN
OWNER_PASSPHRASE=$(openssl rand -base64 24)
EOF
chmod 600 ~/.config/chance-of-files/*.secrets
unset INTERNAL_TOKEN
grep OWNER_PASSPHRASE ~/.config/chance-of-files/auth.secrets      # put this in your password manager
```

Anyone who has the passphrase can connect their Claude to your files, so keep it long and random. It's also the real defence against guessing: attempts are rate limited per address, which slows down one guesser but not one with many addresses, and 32 random characters can't be guessed either way.

## 8. Deploy the core Worker

In `crates/worker/wrangler.toml`, uncomment the `[[vpc_services]]` block and paste the service ID from step 6:

```toml
[[vpc_services]]
binding = "PCLOUD_BACKEND"
service_id = "<SERVICE_ID>"
remote = true
```

Leave the block in: local development is unaffected, because `BACKEND_URL` in `crates/worker/.dev.vars` takes precedence over the binding.

Then deploy it with its secrets. Wrangler runs `worker-build --release` first, which compiles the Rust to wasm and takes a minute or two the first time.

```bash
(cd crates/worker && wrangler deploy --secrets-file ~/.config/chance-of-files/core.secrets)
```

The core has no public URL (`workers_dev = false`), so there's nothing to open in a browser. `wrangler tail chance-of-files-core` shows its requests once the connector is in use.

## 9. Deploy the auth front

```bash
(cd auth && npm ci && npx wrangler deploy --secrets-file ~/.config/chance-of-files/auth.secrets)
```

Wrangler prints the Worker's URL, `https://chance-of-files.<subdomain>.workers.dev`.

`auth/wrangler.jsonc` binds `OAUTH_KV` without an ID, so this first deploy creates a KV namespace named `chance-of-files-oauth-kv` and binds it. When you deploy from a terminal (not CI), wrangler also writes the new namespace's `id` into `auth/wrangler.jsonc` and says you can keep or discard it. In a clone of the public repository, discard it with `git restore auth/wrangler.jsonc`: later deploys without an ID reuse the namespace the deployed Worker is already bound to, while an ID in the shared file would break deploys from anyone else's account (and get in the way of your next `git pull`). The ID isn't a secret, so keeping it in a private fork is fine too. One catch: once the auth Worker is deleted (`wrangler delete`), there's no binding left to reuse, and its next deploy provisions a namespace again. To keep the old one and its sign-ins, put its `id` in `auth/wrangler.jsonc` for that deploy; otherwise delete it first, as in [Teardown](#teardown).

The auth front declares its secrets as required, so wrangler refuses its first deploy without them. Later deploys of either Worker reuse the secrets already set, so they don't need `--secrets-file`, and you can delete the two `.secrets` files once both Workers are up (the passphrase is in your password manager).

Check the public side, with your subdomain in place of `<subdomain>`:

```bash
URL=https://chance-of-files.<subdomain>.workers.dev
curl -s $URL/healthz; echo                                         # ok
curl -si -X POST $URL/mcp | grep -i -e '^HTTP' -e www-authenticate # 401, Bearer resource_metadata="…"
curl -s $URL/.well-known/oauth-protected-resource/mcp; echo        # "resource": "$URL/mcp"
curl -s $URL/.well-known/oauth-authorization-server; echo          # client_id_metadata_document_supported: true
curl -s -o /dev/null -w '%{http_code}\n' -X POST $URL/register \
  -H 'Content-Type: application/json' -d '{}'                     # 403: your address isn't in DCR_ALLOWED_CIDRS
```

## 10. Optional: a custom domain

If you have a zone on Cloudflare, you can serve the auth front on your own hostname, for example `chance-of-files.example.com`. In `auth/wrangler.jsonc`:

```jsonc
"routes": [{ "pattern": "chance-of-files.example.com", "custom_domain": true }],
"workers_dev": false,
"vars": {
  "PUBLIC_URL": "https://chance-of-files.example.com",
  // the other vars unchanged
}
```

Redeploy with `npx wrangler deploy` in `auth/`. The connector URL is then `https://chance-of-files.example.com/mcp`. Turning `workers_dev` off leaves one hostname; if you keep both, `PUBLIC_URL` decides which one the OAuth metadata names, and the other answers `421`. The core needs no change.

If you switch hostnames after adding the connector, remove the connector in Claude and add it again: the URL is part of every token.

## 11. Add the connector in Claude

On claude.ai (or in the desktop app):

1. Open **Customize > Connectors** and select **Add custom connector**.
2. Name: `pCloud`. URL: `https://chance-of-files.<subdomain>.workers.dev/mcp`, exactly, with no trailing slash.
3. Leave the OAuth client ID and secret empty. If the dialog asks how Claude identifies itself, choose **Use Claude's published identity** (CIMD). **Register automatically** (DCR) is refused by default; if CIMD doesn't work for you, redeploy the auth front with Anthropic's range allowed (`npx wrangler deploy --var 'DCR_ALLOWED_CIDRS:160.79.104.0/21'`) and try again.
4. Select **Add**, then **Connect**. Your Worker's sign-in page opens: enter the passphrase and approve. You land back in claude.ai and the connector shows **Connected**.

Open the connector's page: **Tool permissions** should list `list_folder`, `get_file_info`, `read_text_file`, `read_image` and `search_files`. They're all read-only, so **Always allow** is reasonable.

Try it in a chat: **+ > Connectors**, turn on pCloud, and ask "What's in the top folder of my pCloud?".

The same connector appears in the Claude desktop and mobile apps. For Claude Code:

```bash
claude mcp add --transport http pcloud https://chance-of-files.<subdomain>.workers.dev/mcp
```

then run `/mcp` in Claude Code to sign in. It uses a loopback redirect, which the auth front allows, and identifies itself with CIMD, so it needs no registration.

A client that registers with DCR instead of using CIMD (the MCP Inspector, for example) does so from your own address, and the deployed auth front refuses that with `403`. Open registration to everyone for the few minutes it takes, then close it again; the client keeps working afterwards, since it registers only once:

```bash
(cd auth && npx wrangler deploy --var 'DCR_ALLOWED_CIDRS:*')   # anyone may register
# add the client and sign in, then:
(cd auth && npx wrangler deploy)                                # back to the configured ranges
```

## 12. Check it in Claude Duet

Duet uses the connectors on your claude.ai account; there's nothing to add. Ask it something like "List my pCloud Library folder" or "Read notes/todo.md from pCloud and summarize it".

If Duet says it can't reach pCloud, first check that the connector shows **Connected** in claude.ai. Duet is a research preview: it uses custom connectors when you talk to it, but its scheduled and background runs may not.

## Troubleshooting

Two terminals with `wrangler tail chance-of-files` and `wrangler tail chance-of-files-core` show what reaches each Worker, and `journalctl --user -u chance-of-files-tunnel -u chance-of-files-webdav -f` shows the laptop side.

### Connect loops back to sign-in, or every tool call fails with 401

- **The Workers' `INTERNAL_TOKEN`s differ.** The core then answers `401` to every forwarded request, and Claude reads that as an expired token. Put one new value on both:

  ```bash
  T=$(openssl rand -hex 32)
  (cd crates/worker && printf %s "$T" | wrangler secret put INTERNAL_TOKEN)
  (cd auth && printf %s "$T" | npx wrangler secret put INTERNAL_TOKEN)
  unset T
  ```

- **The URL in Claude isn't the protected resource.** It must equal `<public URL>/mcp` exactly: no trailing slash, `https`, and the same hostname as `PUBLIC_URL` (or the one you deployed to). Check `curl -s $URL/.well-known/oauth-protected-resource/mcp`.
- **Wrong passphrase**: the sign-in page says so. After 5 tries in a minute it makes you wait.

### "Couldn't reach the MCP server"

Run the `curl` checks from step 9 from a network other than Cloudflare's. `POST /mcp` must answer `401` with a `WWW-Authenticate` header, and both `.well-known` documents must answer `200`. A `404` there usually means the URL has a typo or the auth front didn't deploy.

### `initialize` fails with `403 Origin not allowed`

The core refuses requests whose `Origin` header it doesn't allow, as MCP asks servers to (it stops web pages from reaching the server through your browser). `https://claude.ai` and `https://claude.com` are always allowed, and requests with no `Origin`, like the ones Claude's servers and Claude Code send, always pass. `wrangler tail chance-of-files-core` logs the origin it refused. If that's a client you use, such as a browser-based tool on another origin, add it to `ALLOWED_ORIGINS` in `crates/worker/wrangler.toml` (comma-separated, for example `"https://inspector.example.com"`) and deploy the core again.

### `500 server misconfigured: no backend`

The deployed core has no `PCLOUD_BACKEND` binding: the `[[vpc_services]]` block in `crates/worker/wrangler.toml` was commented out when you last deployed it. Put it back as in [step 8](#8-deploy-the-core-worker) and deploy the core again. (`BACKEND_URL`, the other way to name a backend, lives only in `.dev.vars` for local development.)

### Tools fail with a backend error or time out

- **The laptop is asleep, offline or off.** That's the proof-of-concept trade-off; tools work again a few seconds after it wakes.
- **pCloud Drive isn't running.** `systemctl --user status chance-of-files-webdav` shows the start check failing; start pCloud Drive.
- **The tunnel is down or on http2.** The logs should show `protocol=quic`. If cloudflared can't connect, the network is blocking outbound UDP (port 7844), as some office and hotel networks do. Don't switch to `--protocol http2`: Workers VPC doesn't work over it. Use another network.
- **rclone rejects the password** (its log shows failed logins): `BACKEND_PASSWORD` on the core doesn't match `RCLONE_PASS`. Fix it with `(cd crates/worker && printf %s "$(sed -n 's/^RCLONE_PASS=//p' ~/.config/chance-of-files/webdav.env)" | wrangler secret put BACKEND_PASSWORD)`.
- **The VPC service points at the wrong place**: `wrangler vpc service get <SERVICE_ID>` should show tunnel ID, `127.0.0.1` and port `8080`.

### "Permission denied" on some files

That's the Crypto Folder (unreadable while locked) or a file pCloud Drive can't open. Serve a folder outside the Crypto Folder, or, when you serve the whole drive, hide it with the [filter file](#serving-the-whole-drive).

### `search_files` says "turned off" or "too many entries"

- **"Search is turned off under ..."**: the search path is at or inside a `SEARCH_SKIP` folder. That's on purpose; Claude can browse it with `list_folder`. To change the list, `wrangler secret put SEARCH_SKIP` again (or `wrangler secret delete SEARCH_SKIP`).
- **"too many entries to go through at once"** for a search from `/`: a folder in the tree is too big to search in one call. Add it to `SEARCH_SKIP` ([Serving the whole drive](#serving-the-whole-drive)). Entries are matched against folder paths as `list_folder` shows them, ignoring case and accents, so copy the path from a listing. An entry that isn't a valid path (one with `..` or a control character in it) is ignored, and `wrangler tail chance-of-files-core` logs a warning naming it by position ("ignoring entry 2"); an entry that is valid but names no folder is ignored without a warning.

### Changes don't show up

- Claude caches a server's metadata and tool list for about 5 minutes. After redeploying, wait, or use the connector's **⋯ > Refresh tool list**, or remove and re-add it.
- New files in pCloud appear within about a minute: rclone caches folder listings (`--dir-cache-time 1m`), and pCloud Drive has to sync them first.

### Sign-in succeeds, but the card stays on Connect or the tools never appear

First check the server's side: run `wrangler tail chance-of-files` while you connect. After `/authorize` and `/token` you should see a few `POST /mcp` requests (`initialize`, the `initialized` notification, `tools/list`) answered `Ok`. If they are, the server has done its part, and you've most likely hit one of two known claude.ai problems:

- [anthropics/claude-ai-mcp#831](https://github.com/anthropics/claude-ai-mcp/issues/831): a stateless Worker's connector never leaves **Connect**, although discovery, sign-in, `initialize` and `tools/list` all succeed.
- [anthropics/claude-ai-mcp#476](https://github.com/anthropics/claude-ai-mcp/issues/476): the connector shows **Connected** and lists its tools, but web chats never offer them to the model.

Workarounds people report: use the desktop or mobile app (or Claude Code); start the conversation on mobile and continue on the web; **⋯ > Refresh tool list** on the connector; turn the connector on for the chat with **+ > Connectors**; or remove the connector and add it again.

## Updating

Deploy the core first, then the auth front:

```bash
git pull
(cd crates/worker && wrangler deploy)
(cd auth && npm ci && npx wrangler deploy)
```

`git pull` keeps your `[[vpc_services]]` edit to `crates/worker/wrangler.toml`; if it refuses because that file changed upstream too, `git stash`, pull, then `git stash pop`. If it complains about `auth/wrangler.jsonc`, that's the KV namespace ID wrangler wrote on the first deploy: `git restore auth/wrangler.jsonc` (see [step 9](#9-deploy-the-auth-front)).

If the systemd units changed: copy them again, `systemctl --user daemon-reload`, and restart both services.

## Renaming from pcloud-mcp

This project was called pcloud-mcp until 2026-10-31; the GitHub repository moved from `tywrr/pcloud-mcp` to `tywrr/chance-of-files`, and GitHub redirects the old URLs. Every name changed with it, so a deployment made before then needs a few steps. Nothing about the pCloud side changes: the tunnel, its token and the VPC service stay as they are, and so does the `service_id` in your `[[vpc_services]]` edit.

1. **Point your clone at the new URL** (optional, the redirect works): `git remote set-url origin https://github.com/tywrr/chance-of-files`, then `git pull`.
2. **Move the config folder** and fix the paths inside it: `mv ~/.config/pcloud-mcp ~/.config/chance-of-files`, then in `webdav.env` rename `PCLOUD_MCP_DIR` to `CHANCE_OF_FILES_DIR` and change any `RCLONE_FILTER_FROM=` path. `deploy/k8s/apply.sh` reads `CHANCE_OF_FILES_CONFIG` now, not `PCLOUD_MCP_CONFIG`.
3. **Deploy the Workers under their new names**, core first, as in steps 8 and 9 with their `--secrets-file` first deploys (`core.secrets` and `auth.secrets` are in the moved folder). The auth Worker is now `chance-of-files`, so it gets a new URL, `https://chance-of-files.<subdomain>.workers.dev`, and a new, empty KV namespace: every client signs in again. If you set `PUBLIC_URL` or a custom domain route, update it.
4. **Re-add the connector in Claude**: remove the old one and add the new `/mcp` URL as in step 11. A custom domain moved to the new Worker keeps its URL, but the connector still has to sign in again.
5. **The backend.** On the laptop, swap the units: `systemctl --user disable --now pcloud-mcp-tunnel pcloud-mcp-webdav`, remove `~/.config/systemd/user/pcloud-mcp-*.service`, then install and start the `chance-of-files-*` units as in steps 3 and 5. On Kubernetes, `deploy/k8s/apply.sh` creates the `chance-of-files` namespace; its default rclone remote is now `chance-of-files`, so run it as `RCLONE_REMOTE=pcloud-mcp deploy/k8s/apply.sh` (or rename the remote with `rclone config`). Once the new pod reports its tunnel connections, `kubectl delete namespace pcloud-mcp`, so only one connector serves the tunnel.
6. **Delete the old Workers and KV namespace**: `npx wrangler delete --name pcloud-mcp`, `wrangler delete --name pcloud-mcp-core`, then `wrangler kv namespace list` to find `pcloud-mcp-oauth-kv` and `wrangler kv namespace delete --namespace-id <ID>`.

## Rotating secrets

| Secret | How |
|---|---|
| `OWNER_PASSPHRASE` | `(cd auth && npx wrangler secret put OWNER_PASSPHRASE)`. Stops new sign-ins with the old one; connections already approved keep working (refresh tokens last 30 days from last use) until you [sign them out](#signing-out-every-client). |
| `INTERNAL_TOKEN` | The snippet under [Connect loops back to sign-in](#connect-loops-back-to-sign-in-or-every-tool-call-fails-with-401). Requests fail for the few seconds between the two `put`s. |
| `BACKEND_PASSWORD` | Change `RCLONE_PASS` in `webdav.env`, `systemctl --user restart chance-of-files-webdav`, then put the same value with `wrangler secret put BACKEND_PASSWORD` in `crates/worker`. |
| Tunnel token | In the dashboard, open **Networking > Tunnels** (the same tunnels Workers VPC lists), select the tunnel, then **Rotate token**. Select **Add replica** to see the install command with the new token, and copy only the `eyJ…` token into `~/.config/chance-of-files/tunnel-token` as in step 4. Then `systemctl --user restart chance-of-files-tunnel`. Until that restart the running connector keeps its connections, but the old token can't open new ones. The tunnel ID doesn't change, so the VPC service needs nothing. |

### Replacing the tunnel

If you ever need a new tunnel rather than a new token, create it as in step 4, save its token to `tunnel-token`, and restart `chance-of-files-tunnel`. Then point the VPC service at it before deleting the old one; tools fail in between. `wrangler vpc service update` replaces the whole service definition, so give it every field, not just the new tunnel ID (`wrangler vpc service update --help` lists them):

```bash
wrangler vpc service update <SERVICE_ID> --name pcloud-laptop --type http --tunnel-id <NEW_ID> --ipv4 127.0.0.1 --http-port 8080
wrangler vpc service get <SERVICE_ID>      # the new tunnel ID, 127.0.0.1, port 8080
```

Once `get` shows the new tunnel and Claude can read files again, delete the old tunnel in **Networking > Tunnels**.

### Signing out every client

Every approved connection (claude.ai, Claude Code, Inspector) is a grant in `OAUTH_KV`. Deleting the grants and tokens signs them all out; each has to **Connect** again with the passphrase. With no ID in `auth/wrangler.jsonc`, `--binding OAUTH_KV` finds the namespace through the deployed Worker's binding.

```bash
(
  cd auth
  npx wrangler kv key list --binding OAUTH_KV --remote --prefix grant: > grants.json
  npx wrangler kv key list --binding OAUTH_KV --remote --prefix token: > tokens.json
  npx wrangler kv bulk delete --binding OAUTH_KV --remote grants.json
  npx wrangler kv bulk delete --binding OAUTH_KV --remote tokens.json
  rm grants.json tokens.json
)
```

## Teardown

1. In Claude: **Customize > Connectors**, pCloud, **Remove**.
2. The Workers: `(cd auth && npx wrangler delete)` and `(cd crates/worker && wrangler delete)`.
3. The KV namespace: find `chance-of-files-oauth-kv` with `wrangler kv namespace list`, then `wrangler kv namespace delete --namespace-id <ID>`.
4. The VPC service: `wrangler vpc service list`, then `wrangler vpc service delete <SERVICE_ID>`.
5. The tunnel: delete it in **Workers VPC > Tunnels** (or **Networking > Tunnels**).
6. The laptop:

   ```bash
   systemctl --user disable --now chance-of-files-tunnel chance-of-files-webdav
   rm ~/.config/systemd/user/chance-of-files-tunnel.service ~/.config/systemd/user/chance-of-files-webdav.service
   systemctl --user daemon-reload
   rm -r ~/.config/chance-of-files
   ```

## Configuration reference

**Core Worker** (`crates/worker/wrangler.toml`; secrets with `wrangler secret put`, locally in `crates/worker/.dev.vars`):

| Name | Kind | Default | Meaning |
|---|---|---|---|
| `INTERNAL_TOKEN` | Secret | (required) | Every `/mcp` request must carry `Authorization: Bearer <INTERNAL_TOKEN>`; otherwise `401`. Same value as the auth front's |
| `PCLOUD_BACKEND` | Workers VPC service binding | (commented out in the repository) | The private route to rclone through the tunnel; see step 8 |
| `BACKEND_URL` | Var, local only | `http://127.0.0.1:8081` in `.dev.vars.example` | Where WebDAV requests go in local development. When set, it wins over `PCLOUD_BACKEND`, so it belongs in `.dev.vars` only, never in `wrangler.toml`. With neither, `/mcp` answers `500 server misconfigured: no backend` |
| `BACKEND_USER` | Var | `mcp` | rclone's Basic auth user (`RCLONE_USER`) |
| `BACKEND_PASSWORD` | Secret | (none) | rclone's Basic auth password (`RCLONE_PASS`) |
| `ALLOWED_ORIGINS` | Var | (none) | Comma-separated browser origins allowed to call `/mcp`, on top of `https://claude.ai` and `https://claude.com`, which are always allowed. Requests without an `Origin` header, like Claude's, always pass; others get `403` and are logged |
| `MAX_READ_BYTES` | Var | `100000` | Most bytes per `read_text_file` call |
| `MAX_IMAGE_BYTES` | Var | `1572864` (1.5 MiB) | Largest image `read_image` returns. The default keeps base64-encoding within the free plan's 10 ms of CPU; raise it on Workers Paid |
| `MAX_LISTING_BYTES` | Var | `400000` | Most WebDAV `PROPFIND` answer bytes the core parses per request: one folder for `list_folder`, the whole tree under the path for `search_files` (or, around `SEARCH_SKIP` folders, all its subfolder searches together). The core stops reading rclone's answer there, so a bigger one costs no more, and stops asking once a request has received twice this much in all, unusable answers included. Past it, the tool error tells Claude to narrow the path, rather than the Worker running out of CPU; raise it on Workers Paid |
| `SEARCH_LIMIT` | Var | `50` | Most results per `search_files` call |
| `SEARCH_SKIP` | Secret or var | (none) | Comma-separated folders `search_files` doesn't go into, such as `/Backups/Immich`: a search inside one is refused (Claude browses it with `list_folder`), and a search above one goes around it. Set it as a secret to keep folder names out of `wrangler.toml`; entries are matched against folder paths as `list_folder` shows them, ignoring case and accents; malformed entries are ignored with a warning in `wrangler tail`. See [Serving the whole drive](#serving-the-whole-drive) |
| `GUIDE_PATH` | Var | `/AGENTS.md` | A file whose first 8 KiB go into the instructions Claude gets when it connects, so you can say what each folder is for. A missing or unreadable file is left out quietly; the empty string turns the guide off |

Routes: `/mcp` is the MCP endpoint, `GET /healthz` answers `200 ok` with no auth and no backend call, and everything else is `404`.

**Auth front** (`auth/wrangler.jsonc`; secrets with `npx wrangler secret put`, locally in `auth/.dev.vars`):

| Name | Kind | Default | Meaning |
|---|---|---|---|
| `INTERNAL_TOKEN` | Secret | (required) | Sent to the core on every forwarded request |
| `OWNER_PASSPHRASE` | Secret | (required) | What the sign-in page asks for: 12 characters or more, 32 random ones from step 7. The real defence against guessing |
| `PUBLIC_URL` | Var | (empty: the request's origin) | The public origin, for example `https://chance-of-files.example.com`; the protected resource is `<PUBLIC_URL>/mcp` |
| `ALLOWED_REDIRECT_URIS` | Var | claude.ai and claude.com callbacks, `loopback` | Where sign-in codes may be sent. `loopback` allows `http://localhost` and `http://127.0.0.1` on any port, for Claude Code and MCP Inspector |
| `DCR_ALLOWED_CIDRS` | Var | `127.0.0.0/8`, `::1` | Comma-separated address ranges that may register a client at `POST /register` (Dynamic Client Registration). The default is loopback only, for local development; add Anthropic's `160.79.104.0/21` only if claude.ai falls back to DCR (anyone using Claude can send requests from it); `*` allows anyone. Other addresses get `403` with an OAuth error. CIMD clients never register |
| `OAUTH_KV` | KV binding | created on first deploy as `chance-of-files-oauth-kv` | Clients, grants and token hashes. Keep its ID out of the shared config; see step 9 |
| `MCP_CORE` | Service binding | `chance-of-files-core` | The core Worker |

Endpoints: `/mcp`, `/authorize`, `/token`, `/register` (from `DCR_ALLOWED_CIDRS` only), `/.well-known/oauth-authorization-server`, `/.well-known/oauth-protected-resource` (and `…/mcp`), `/healthz`.
