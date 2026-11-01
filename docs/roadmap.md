# Roadmap

The phases of [design v2](design/v2.md#phased-delivery), in order. Nothing has a date; phase 5 waits on pCloud.

| Release | Ships | Done when |
|---|---|---|
| **0.1** | Two Workers + the laptop's pCloud Drive mount; five read tools. Frozen at tag [`v0.1.0`](https://github.com/tywrr/chance-of-files/tree/v0.1.0) | (shipped) |
| **0.2: native parity** | `crates/server`, `crates/rclone`; the same five tools on a native core in your own cluster; `files:read` / `files:write` scopes; auth Worker on a VPC service; kustomize base and `build-image.sh` | The old two-Worker stack is deleted and everything runs on the Workers free plan (shipped, tag [`v0.2.0`](https://github.com/tywrr/chance-of-files/tree/v0.2.0)) |
| **0.3: index + audit** (done) | `crates/index` (SQLite, WAL): a metadata index of the whole drive, walked per root (every 30 min by default) by splitting the tree around hidden paths and child roots, with streamed parsing of big listings; walk guards (own-write and tombstone races, a 5% mass-removal hold that `refresh_index confirm=true` applies); `folder_totals` and lazy duplicate hashing; `folder_stats`, `query_files`, `find_duplicates`, `recent_changes`, `type_breakdown`, `index_status`, `refresh_index`; `list_folder` and `search_files` from the index (whole drive, sort and cursors); a backend health check every 5 min | Claude answers "where did my 408 GiB go" in one or two calls (shipped, tag [`v0.3.0`](https://github.com/tywrr/chance-of-files/tree/v0.3.0)) |
| **0.4: writes + links** (v0.4.0) | `create_folder`, `move`, `copy`, `write_file`, `delete` (to pCloud's trash only), all `destructiveHint` and behind `files:write` and `policy.protected`; folder moves, copies and deletes as rclone jobs in `state.db` with `job_status`, re-walking what they touched; index write-through after each confirmed write; expiring links signed with `LINK_KEY` (15 min default, 24 h max, 3× byte cap, /24 binding, `link_log`): single files with `Range`, streamed ZIP64 zips up to 10 GB, chunked uploads in parts ≤ 90 MiB staged on the volume with a space reservation; `create_download_link`, `create_upload_link`, `list_links`, `revoke_link` | A 5 GB folder round-trips to Duet's computer and back |
| **0.5: mirror + full text** | A one-way local mirror driven by the index; text extraction (PDF, DOCX/XLSX/PPTX, EML, MBOX); `read_document`, `search_content`; `read_image` downscaling | Claude finds a 2019 email by sender and phrase |
| **pCloud API backend** | A `pcloud-sdk` crate and a `Storage` backend that calls pCloud directly: change feed (`diff`) instead of polling, server-side thumbnails (`getthumb`) | When pCloud approves this project's OAuth app |

The seams for what's left are already in: the `Storage` and `StorageWrite` traits (a pCloud API backend implements the same two), the index's change log (which the mirror will follow), the data volume at `/var/lib/chance-of-files` (`text.db` and `mirror/` join `index/`, `state.db` and `staging/` there in 0.5), and the auth Worker's single `CORE` fetch path.

## Not planned

- Permanent deletes, emptying the trash, pCloud Rewind.
- Two-way sync.
- Resumable zips: they'd need every CRC before the first byte, which means reading the whole folder from pCloud twice. Per-file links resume instead.
- Photo contents, OCR, embeddings.
- Public sharing links (Chance of Files' links are private and expire; pCloud's own MCP server makes public ones).
- Workers Paid, or a Workers-only core (0.1 stays tagged for that).

## Smaller things

- Published multi-arch images on ghcr.io, built by CI on tags (until then, `deploy/build-image.sh`).
- Fuller MCP 2026-07-28 support as Claude adopts it: `outputSchema` for every tool (the audit tools already return `structuredContent`), resource links.
- If Workers VPC stops being free: a tunnel public hostname behind a Cloudflare Access service token, as a config switch in the auth Worker ([design, Risks](design/v2.md#risks)).
