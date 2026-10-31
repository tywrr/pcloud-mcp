# Roadmap

The phases of [design v2](design/v2.md#phased-delivery), in order. Nothing has a date; phase 5 waits on pCloud.

| Release | Ships | Done when |
|---|---|---|
| **0.1** | Two Workers + the laptop's pCloud Drive mount; five read tools. Frozen at tag [`v0.1.0`](https://github.com/tywrr/chance-of-files/tree/v0.1.0) | (shipped) |
| **0.2: native parity** (in progress) | `crates/server`, `crates/rclone`; the same five tools on a native core in your own cluster; `files:read` / `files:write` scopes; auth Worker on a VPC service; kustomize base and `build-image.sh` | The old two-Worker stack is deleted and everything runs on the Workers free plan |
| **0.3: index + audit** | `crates/index` (SQLite): a metadata index of the whole drive, walked every 30 min by splitting the tree around hidden paths and child roots; `folder_stats`, `query_files`, `find_duplicates`, `recent_changes`, `type_breakdown`, `index_status`, `refresh_index`; a backend health check | Claude answers "where did my 408 GiB go" in one or two calls |
| **0.4: writes + links** | `create_folder`, `move`, `copy`, `write_file`, `delete` (to pCloud's trash only), all behind `files:write` and `policy.protected`, with Claude asking before each; async jobs for folders; expiring download links (single files with `Range`, streamed zips) and chunked upload links through the auth Worker | A 5 GB folder round-trips to Duet's computer and back |
| **0.5: mirror + full text** | A one-way local mirror driven by the index; text extraction (PDF, DOCX/XLSX/PPTX, EML, MBOX); `read_document`, `search_content`; `read_image` downscaling | Claude finds a 2019 email by sender and phrase |
| **pCloud API backend** | A `pcloud-sdk` crate and a `Storage` backend that calls pCloud directly: change feed (`diff`) instead of polling, server-side thumbnails (`getthumb`) | When pCloud approves this project's OAuth app |

The seams for these are already in 0.2: the `Storage` trait (a write trait joins it in 0.4), scopes on every tool, `Policy` with `protected`, the data volume at `/var/lib/chance-of-files`, and the auth Worker's single `CORE` fetch path.

## Not planned

- Permanent deletes, emptying the trash, pCloud Rewind.
- Two-way sync.
- Photo contents, OCR, embeddings.
- Public sharing links.
- Workers Paid, or a Workers-only core (0.1 stays tagged for that).

## Smaller things

- Published multi-arch images on ghcr.io, built by CI on tags (until then, `deploy/build-image.sh`).
- Fuller MCP 2026-07-28 support as Claude adopts it: `outputSchema` and structured content for the listing tools, resource links.
- If Workers VPC stops being free: a tunnel public hostname behind a Cloudflare Access service token, as a config switch in the auth Worker ([design, Risks](design/v2.md#risks)).
