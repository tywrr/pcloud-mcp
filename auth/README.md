# Chance of Files auth front

The public half of Chance of Files: a TypeScript Worker named `chance-of-files` that Claude connects to. It is an OAuth 2.1 authorization server for one person, the owner, built on [`@cloudflare/workers-oauth-provider`](https://github.com/cloudflare/workers-oauth-provider) 1.2.4. Requests to `/mcp` that carry a valid access token are forwarded to the Rust core in the owner's cluster over the `CORE` binding, a [Workers VPC](https://developers.cloudflare.com/workers-vpc/) service that reaches `127.0.0.1:8080` in the pod through a Cloudflare Tunnel. Claude's token is replaced by the shared `INTERNAL_TOKEN`, and the grant's scopes go along in `X-CoF-Scopes`. The core has no public URL.

| Path | What it does |
|---|---|
| `/mcp` | Needs `Authorization: Bearer <access token>`, else `401` with `WWW-Authenticate: Bearer … resource_metadata="…", scope="files:read"`. Forwarded to the core with `Authorization: Bearer <INTERNAL_TOKEN>`, `X-CoF-Scopes`, `X-CoF-Public-Url`, no `Cookie` and no client-sent `X-CoF-*` header; the core's response comes back unchanged. A token without `files:read` (refreshed down to less) gets `403 insufficient_scope` |
| `/authorize` | The sign-in page: who is asking, where access goes, the write checkbox, a passphrase field |
| `/token` | Code exchange, refresh, revocation, for public clients and DCR clients with a secret. Client assertions (`private_key_jwt`) get `401 invalid_client` |
| `/register` | Dynamic Client Registration (RFC 7591), only from the addresses in `DCR_ALLOWED_CIDRS`; others get `403` |
| `/.well-known/oauth-authorization-server` | RFC 8414 metadata |
| `/.well-known/oauth-protected-resource/mcp`, `/.well-known/oauth-protected-resource` | RFC 9728 metadata; `resource` is `<origin>/mcp` |
| `/dl/<token>/<name>` | A signed download link (`GET`, `HEAD`, with `Range`), no OAuth. See [Links](#links) |
| `/ul/<token>` | A signed upload link: `PUT` one part (≤ 90 MiB, with `Content-Length` and `Content-Range`), `HEAD` for the committed offset. See [Links](#links) |
| `/healthz` | `ok`, without auth |

Everything else is `404`.

## Scopes

| Scope | Granted | Lets Claude |
|---|---|---|
| `files:read` | Always | List, read and search |
| `files:write` | Only when the owner ticks **Also allow changes (create, move, upload, delete)** on the sign-in page | Use the write tools (from phase 3; deletes go to pCloud's trash) |
| `offline_access` | When the client asks | Nothing more: refresh tokens are issued either way. Never forwarded |

- **The owner decides `files:write`, not the client.** The checkbox is unticked on every page this Worker renders, including the one after a wrong passphrase, so a cross-site post can't pre-tick it. A client that asks for `files:write` gets read only unless the box is ticked, and ticking it gives write access even to a client that asked only for `files:read` (Claude asks for what the `401` names, `files:read`). The form field is trusted for this only on a post that also carried the right passphrase.
- **The grant keeps its scopes.** They are stored on the grant (in the library's grant record, with `scope`), every refresh issues a token with the same scopes, and a client can only narrow them with the `scope` parameter on refresh. To add or drop write access, the owner reconnects the connector.
- **What the core sees.** The Worker deletes every `X-CoF-*` header the client sent, then sets `X-CoF-Scopes` to the token's file scopes, space-separated: `files:read` or `files:read files:write`. The core trusts the header only on requests with `INTERNAL_TOKEN`, hides the write tools from `tools/list` without `files:write`, and refuses calls to them.
- **Discovery.** The authorization server metadata lists `files:read files:write offline_access` in `scopes_supported`. The protected resource metadata and the `401` challenge name only `files:read`, the minimum a client needs; write access is the owner's choice at sign-in, not a step-up.

## Sign-in and safety

- **One passphrase.** `OWNER_PASSPHRASE` is compared in constant time (SHA-256 digests with `crypto.subtle.timingSafeEqual`). It must be at least 12 characters; use a long random one (`openssl rand -base64 24`). Anyone who knows it can connect a client.
- **Redirect allowlist.** Dynamic registration lets anyone register a client, so codes only go to the URIs in `ALLOWED_REDIRECT_URIS`: by default Claude's two callbacks plus `http://localhost` and `http://127.0.0.1` on any port and path, for Claude Code and the MCP Inspector. A request for any other redirect URI is refused before the page is shown, and a registration with no allowed redirect URI is refused outright.
- **PKCE with S256 for every client,** including confidential ones (the library only requires it from public clients).
- **Consent without server state.** `GET /authorize` validates the request and shows the form, and stores nothing: no KV record, no cookie. The form has no `action`, so it posts back to the same URL with the authorization request still in the query string. `POST /authorize` checks the passphrase (rate-limited, constant-time) first, then parses and checks that request again from scratch (client, redirect URI against the allowlist, PKCE) and completes the authorization; the form body contributes only the passphrase and the write checkbox, and the checkbox counts only alongside the right passphrase. **Deny** needs no passphrase: it sends `error=access_denied` (with `state` and `iss`) to the request's redirect URI, after the same allowlist check.
- **CSRF and framing.** The passphrase is the CSRF secret: another site can make the owner's browser post the form, but without the passphrase it can only deny. Pages can't be framed (`frame-ancestors 'none'`, `X-Frame-Options: DENY`), so no site can overlay the real form to trick the owner into typing the passphrase and clicking Allow. Pages also send `Content-Security-Policy: default-src 'none'` (plus a per-response style nonce and `form-action` limited to this origin and the client's redirect origin), `Cache-Control: no-store` and `Referrer-Policy: no-referrer`.
- **The KV write budget.** The free plan allows 1,000 KV writes a day, and when they run out, sign-in and token refresh fail until 00:00 UTC. So nothing an anonymous caller can reach writes to KV: the sign-in page stores nothing, only a correct passphrase creates a grant, and only `DCR_ALLOWED_CIDRS` may register clients. That defaults to loopback only, for `wrangler dev`: Claude identifies itself with its Client ID Metadata Document and doesn't register. Anthropic's outbound range (`160.79.104.0/21`, [documented here](https://platform.claude.com/docs/en/api/ip-addresses)) is left out on purpose, because anyone using Claude can send requests from it; add it only if claude.ai ever falls back to DCR for your server. The registration endpoint stays advertised for that case. The library caches the Client ID Metadata Documents it fetches with the Cache API, never in KV. It would also accept `private_key_jwt` from a CIMD client and record each assertion's `jti` in KV before checking the code or refresh token, so anyone who publishes a metadata document with their own key could spend a write per request; Claude's clients authenticate as public clients, so `/token` refuses client assertions and the metadata doesn't advertise them.
- **Rate limits** (Workers Rate Limiting binding, per IP, with all of an IPv6 caller's /64 counted as one address; approximate and per Cloudflare location): 5 passphrase attempts a minute; 20 sign-in page views, denials or registrations a minute. They slow down guessing and floods; they are not what protects the KV budget, since a caller with many addresses gets around them.
- **Tokens.** Access tokens last 1 hour. Refresh tokens rotate on every use; a grant lasts 30 days and each refresh extends it by 30 days. Tokens are stored in KV only as hashes.
- **Discovery.** Client ID Metadata Documents (needs the `global_fetch_strictly_public` compatibility flag) and Dynamic Client Registration are both on, and `none` is among the token endpoint auth methods, which is what Claude needs to pick CIMD.

## Links

`create_download_link` and `create_upload_link` (core, 0.4) return URLs on this Worker, so a client that holds no OAuth token, such as Duet's computer with `curl`, can move files bigger than a tool result. The token in the path is the credential: `<id>.<exp>.<mac>`, where `id` is 128 random bits, `exp` the expiry in Unix seconds and `mac` the first 128 bits of HMAC-SHA256 with `LINK_KEY` (its UTF-8 bytes) over `"<kind>.<id>.<exp>"`, where `kind` is `dl` or `ul`; `id` and `mac` are unpadded base64url. The core mints and owns links; this Worker only pre-checks them, so junk never reaches the cluster.

- **Checks here, in order:** method (`GET`/`HEAD` on `/dl`, `PUT`/`HEAD` on `/ul`, else `405`), `LINK_LIMITER` (60 requests a minute per IP or IPv6 /64, else `429`), token shape (`404`), MAC in constant time (`403`; the kind is signed, so an upload token is no download link), expiry (`410`). For a `PUT`: `Content-Length` is required (`411`) and at most 90 MiB (`413`), and a `Content-Range` must span exactly that many bytes (`413` if wider than 90 MiB, `400` if it disagrees). Cloudflare caps request bodies at 100 MB on the free plan, so parts of 90 MiB are the only way to upload.
- **Checks in the core** (authoritative): the link's row (revoked, used up, completed), the `/24` (`/64`) binding to the first client, the per-link byte cap, ranges against the real size (`416`), upload offsets and the final size and hash.
- **What the core sees:** the request as sent, with `Authorization: Bearer <INTERNAL_TOKEN>`, `X-CoF-Client-IP` set to `CF-Connecting-IP`, and no cookies or client-sent `X-CoF-*` headers. Bodies stream both ways; nothing is buffered, so a 2 GiB download or a 90 MiB part costs the Worker almost no CPU.
- **Download headers.** Links share this origin with the sign-in page, so every link response gets `X-Content-Type-Options: nosniff`, `Content-Security-Policy: default-src 'none'; sandbox; …`, `X-Frame-Options: DENY`, `Cache-Control: no-store` and `Referrer-Policy: no-referrer`, and successful downloads are always `Content-Disposition: attachment` (the core's, or one built from `<name>`) with `application/octet-stream` (or `application/zip`). Cookies from the core are dropped.

`LINK_KEY` must be the same on the Worker and the core. Changing it on both revokes every link at once.

## Configuration

| Name | Kind | Meaning |
|---|---|---|
| `INTERNAL_TOKEN` | secret | Same value as the core's `INTERNAL_TOKEN` (the `internal-token` key of the cluster Secret) |
| `OWNER_PASSPHRASE` | secret | The sign-in passphrase (12+ characters) |
| `LINK_KEY` | secret | The HMAC key of `/dl` and `/ul` links; the same value as the core's `LINK_KEY` (in the cluster Secret). At least 32 characters, random, for example `openssl rand -base64 32`. Without it (or shorter) links answer `503` |
| `PUBLIC_URL` | var | Public origin, for example `https://chance-of-files.example.workers.dev`. Empty (default) uses the origin each request arrives on. When set, requests on any other hostname get `421` |
| `ALLOWED_REDIRECT_URIS` | var | Comma-separated exact redirect URIs, plus `loopback` for the loopback rule. Empty means the defaults |
| `DCR_ALLOWED_CIDRS` | var | Who may call `POST /register`: comma-separated IPv4 or IPv6 CIDRs (a bare address is one host), matched against `CF-Connecting-IP`, or `*` for everyone. Empty means the default, `127.0.0.0/8,::1`. Add `160.79.104.0/21` (Anthropic) only if claude.ai falls back to DCR. Claude Code doesn't need it, since it identifies itself with its own metadata document. A client on your own computer that does use DCR, such as the MCP Inspector, registers from your own address: add it for the moment you connect the client, or use `*` and change it back |
| `CORE_URL` | var | Local development only: a loopback origin such as `http://127.0.0.1:8080` to send `/mcp` to a core on this computer instead of `CORE`. Anything but a loopback `http` origin is refused with `500`, and a deployed Worker can't reach loopback anyway. Empty by default; set it in `.dev.vars` |
| `OAUTH_KV` | KV | Clients, grants, tokens. Created on the first deploy |
| `CORE` | VPC service | The core, through the Workers VPC service that points at `127.0.0.1:8080` over your tunnel. `service_id` is a `<SERVICE_ID>` placeholder in `wrangler.jsonc`; the real one goes in `wrangler.personal.jsonc` |
| `PASSPHRASE_LIMITER`, `SIGNIN_LIMITER`, `LINK_LIMITER` | rate limit | See above and [Links](#links); `namespace_id` must be unique in your account |

The `resource` in the protected-resource metadata is `<public origin>/mcp`, and it must equal the URL you enter in Claude exactly, so add the connector as `https://<your worker host>/mcp`.

## Develop

```bash
npm ci
cp .dev.vars.example .dev.vars
npm run dev          # wrangler dev --local, http://localhost:8788; /mcp goes to CORE_URL
npm run dev:tunnel   # wrangler dev with wrangler.personal.jsonc: /mcp goes through the real tunnel
npm test             # Vitest in workerd; CORE is a stub that echoes what it receives
npm run typecheck    # checks worker-configuration.d.ts is current, then tsc
npm run cf-typegen   # regenerate worker-configuration.d.ts after editing wrangler.jsonc
```

Workers VPC services have no local simulation. `npm run dev` turns remote bindings off, so `CORE` can't be used there (`/mcp` answers `502`) unless `CORE_URL` in `.dev.vars` points at a core on this computer, such as `chance-of-files` from `crates/server` on `http://127.0.0.1:8080`. `npm run dev:tunnel` binds `CORE` remotely to your real VPC service, which needs `wrangler login` and a running pod. The tests never contact Cloudflare: `vitest.config.ts` sets `remoteBindings: false`, and `test/helpers.ts` passes a stub `CORE` in the env.

Point the MCP Inspector at `http://localhost:8788/mcp`; its loopback callback is allowed, and the passphrase is `dev-passphrase` from `.dev.vars.example`. Locally, wrangler sets `CF-Connecting-IP` to the connection's address (`127.0.0.1` or `::1`), so the default `DCR_ALLOWED_CIDRS` lets local clients register. Wrangler keeps a `CF-Connecting-IP` the client sends itself, so set one with curl to try a refusal: `curl -i -X POST -H 'CF-Connecting-IP: 198.51.100.7' -H 'Content-Type: application/json' -d '{"redirect_uris":["http://localhost:1/cb"]}' http://localhost:8788/register`. In production Cloudflare sets the header and clients can't.

## Deploy

The public `wrangler.jsonc` has a `<SERVICE_ID>` placeholder. Deploy from a personal copy, which `.gitignore` keeps out of the repository (back it up somewhere private):

```bash
cp wrangler.jsonc wrangler.personal.jsonc
# In the copy, replace <SERVICE_ID> with your VPC service's id (`npx wrangler vpc service list`).
```

Then, from `auth/`, with the core running in the cluster:

```bash
npx wrangler login
# A new Worker can't hold secrets before its first deploy, and wrangler refuses to deploy
# without the required ones, so the first deploy passes them in a file it reads once:
npx wrangler deploy -c wrangler.personal.jsonc --secrets-file <(printf 'INTERNAL_TOKEN=%s\nOWNER_PASSPHRASE=%s\nLINK_KEY=%s\n' "$INTERNAL_TOKEN" "$OWNER_PASSPHRASE" "$LINK_KEY")
# Later changes:
npx wrangler secret put OWNER_PASSPHRASE -c wrangler.personal.jsonc
# Upgrading a Worker deployed before 0.4: add LINK_KEY (the core's value) before deploying.
npx wrangler secret put LINK_KEY -c wrangler.personal.jsonc
npm run deploy       # wrangler deploy -c wrangler.personal.jsonc
```

After changing `wrangler.jsonc`, make the same change in your copy. The `OAUTH_KV` namespace is still created automatically on the first deploy (no id in either file) and stays linked to the Worker. The Rate Limiting binding is generally available and not documented as paid-only, but it has not yet been deployed on a free account for this project. Workers VPC is free while it is in beta.
