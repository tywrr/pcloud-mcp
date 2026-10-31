# Roadmap

Roughly in order. Nothing here has a date; the first item waits on pCloud.

## pCloud API backend, and a `pcloud-sdk` crate

The big one: read pCloud directly from the Worker, so the laptop, the tunnel and rclone go away.

- **Waiting on**: pCloud approving an OAuth app for this project (requested; app registration is manual since password sign-in was removed in April 2026). See [background.md](background.md#pcloud-api-facts-and-gotchas).
- **Shape**: a new crate implementing `Storage` (`list` → `listfolder`, `stat` → `stat`, `read` → `getfilelink` plus a ranged `GET` from the Worker itself, `search` → `search` or a bounded walk), chosen by config in the core Worker. The tools don't change.
- **Auth**: the auth front grows a second leg: after the passphrase, it sends the owner through pCloud's OAuth once and stores the pCloud token (encrypted, in KV), with the client secret as a Worker secret. US and EU API hosts both supported.
- **`pcloud-sdk` crate**: the pCloud client itself (auth, the methods above, typed errors, both API hosts), runtime-neutral like `chance-of-files-webdav` (generic over the HTTP transport), so it's useful outside this project. The name `pcloud-sdk` is free on crates.io as of 2026-10-29.
- **Keep WebDAV**: the WebDAV backend stays for anyone who would rather serve a local folder, a NAS, or rclone's `pcloud:` remote.

## Write tools, behind a flag

Off by default, and enabled per tool with a config var on the core (for example `WRITE_TOOLS=mkdir,move`), because a read-only connector is a much smaller risk.

| Tool | Notes |
|---|---|
| `create_folder` | |
| `move` / `rename` | Within the served folder only |
| `upload_text_file` | Small text files from Claude; size-capped |
| `trash` | Delete to pCloud's trash, never permanently (Duet already asks before deleting anything) |

With the WebDAV backend these need rclone without `--read-only`, so they'll likely come with the API backend first. MCP tool annotations (`readOnlyHint`, `destructiveHint`) mark them so Claude asks before using them.

## All-Rust OAuth option

An optional OAuth server inside the Rust Worker, for people who want one Worker and one language: PKCE, CIMD (with SSRF-safe fetches), DCR, refresh rotation and hashed tokens in KV, tested against the same flows as the TypeScript front. The TypeScript front stays the default until this has had real use; see [background.md](background.md#alternatives-considered) for why it isn't first.

## Fuller MCP 2026-07-28 support

Today the endpoint is stateless and answers `initialize` for 2025-03-26, 2025-06-18 and 2025-11-25, with the cheap parts of 2026-07-28's stateless requests. Next: the rest of 2026-07-28 as Claude adopts it, `outputSchema` and structured content for the listing tools, and resource links so Claude can refer back to files.

## More file types

- **PDF text extraction** in `read_text_file` (or a `read_document` tool), within the 10 ms CPU budget or behind a size cap; scanned PDFs need OCR and are out of scope for the Worker.
- **Office documents**: text from `.docx`, `.xlsx` and `.pptx` (zipped XML, so feasible in Rust on wasm).
- **Thumbnails**: `read_image` returns a downscaled image when the original is over `MAX_IMAGE_BYTES`, instead of an error. pCloud's API can make thumbnails server-side (`getthumb`), which the API backend should use.

## Smaller things

- Content search, if the pCloud API or an index makes it cheap; today `search_files` matches names only.
- Better errors when the laptop is asleep ("the backend is offline" rather than a timeout).
- Tunnel setup through the Cloudflare API instead of the dashboard, for scripting.
