# Phase 0: rclone spikes

**Date:** 2026-10-31. **Design:** [v2.md](v2.md), phase 0 ("Spikes"), rclone part only. The Cloudflare-path, transfer-size and Duet-egress spikes are separate.
**Setup:** laptop (x86_64), `/usr/bin/rclone` v1.75.2, remote `pcloud-mcp:` (pCloud US, `api.pcloud.com`), home connection. Every measurement used a fresh `rclone rcd --rc-addr=127.0.0.1:15572 --rc-user=spike --rc-pass=<random>`, and each was stopped afterwards. Peak RSS is the process's `VmHWM` from `/proc/<pid>/status`, read just before the process was stopped. `/usr/bin/time` isn't installed on this machine. An idle rcd peaks at **84 MB**. API calls were counted with `--dump headers`. Writes happened only in `pcloud-mcp-test/phase0/`, which now holds only a `README.md`.

rc calls were made with curl:

```bash
curl -s -u spike:$PASS -H 'Content-Type: application/json' -X POST http://127.0.0.1:15572/<method> -d '<json>'
```

## Summary of what changes in the design

1. **Don't run rcd with `--filter-from` (or any directory filter), and don't send per-call `_filter` on walks or folder moves.** Any directory-style filter rule (`- /x/**`) makes rclone stop using pCloud's recursive `ListR` and fall back to one `listfolder` call per folder. It also makes `sync/move` move files one by one instead of making one `renamefolder` call. Walking Immich takes 6.0 s with ListR and 133 s with a filter (6,029 calls, before `--tpslimit`; about 12.5 min at `--tpslimit=8`). The filter doesn't protect anything anyway (item 4), so `Policy` alone does it, and the "second layer" goes.
2. **Walks split the tree themselves instead of using `_filter`.** To walk `/` without Immich and the Crypto Folder, the core lists `/` and `/Backups` non-recursively and lists every other folder recursively (`recurse: true`, no filter). That took 12 API calls and **4.2 s at `--tpslimit=8`**, against 716 calls and **90 s** with `_filter` (16–20 s without a tps limit). Hidden folders are then never fetched at all. With `_filter`, they are fetched and dropped.
3. **`--rc-serve` is fine for ranges, with three caveats.** The core must check ranges against the size itself, because an unsatisfiable range comes back as a broken `206`. It must never let a caller choose the `[fs]` part of the URL, because rc-serve also serves local paths. And it must treat the rc password as root-equivalent inside the rclone container.
4. Confirmed as designed: per-call `_filter` excludes a child root; the filter gap exists (`stat`, `deletefile`, `copyfile` and rc-serve `GET` all go through `--filter-from`); `deletefile`/`purge` call pCloud's `deletefile`/`deletefolderrecursive` (trash, owner to confirm in the web UI); hashes are MD5 + SHA-1; `about` works.

## Results

| # | Question | Result | Meaning for the design |
|---|---|---|---|
| 1a | Recursive list of `Backups/Immich` | **132,648 entries** (126,622 files, 6,026 folders, 380.1 GiB). **5.4 / 5.9 / 6.7 s**, peak RSS **437–441 MB**, 3 API calls (2 path lookups + 1 `listfolder?recursive=1`). Response JSON 35 MB. | Fast enough to walk Immich far more often than every 6 h, if the rcd has no directory filters (see 1d). rclone's 1 Gi limit is fine. The core gets a 35 MB JSON body: parse it as a stream or budget roughly 300 MB for the diff. |
| 1b | Recursive list of `/` excluding Immich via per-call `_filter` `{"ExcludeRule":["/Backups/Immich/**"]}` | **6,617 entries** (5,902 files, 715 folders, 28.1 GiB). **15.5 / 15.8 / 20.3 s**, peak RSS **112–113 MB**. **716 API calls** (one `listfolder` per folder, no `recursive=1`). With `--tpslimit=8`: **89.6 s**. | The exclusion works (1d), but a directory filter turns off ListR. **Change:** walk by splitting (item 2 above). The split walk on the same tree at `--tpslimit=8` made 12 calls (2 plain, 10 `recursive=1`), took **4.2 s** and peaked at 107 MB RSS. |
| 1c | Recursive list of `/`, unfiltered; error 1101 | **139,266 entries**, 6,742 folders, 408.3 GiB. **6.8 / 7.8 / 8.2 s**, peak RSS **506–519 MB**. 6 API calls: 1 `listfolder` on folderid 0 (non-recursive) + 5 `listfolder?recursive=1`, one per top-level folder. **No 1101 error.** | rclone 1.75.2 already works around 1101: its `listAllRootRecursive` (backend/pcloud/pcloud.go) lists root non-recursively, then each top-level folder recursively. The 1101 error itself wasn't reproduced, because that would mean calling the API with rclone's token outside rclone. The core never needs `recursive` on folderid 0. This listing included 593 entries under the Crypto Folder and 40 under the old `.Trash-1000` (metadata only). The split walk skips both. |
| 1d | Recursive list with a global `--filter-from` (production filter file) | `Backups/Immich`: same 132,648 entries, **132.7 s, 6,029 API calls** (per folder), peak RSS 431 MB. | `walk.ListR` refuses ListR when `fi.UsesDirectoryFilters()` (fs/walk/walk.go). Every rule in today's filter file (`- /Crypto Folder/**`, …) counts as one. **Change:** no `--filter-from` on the v2 rcd (D2 config line drops it). |
| 1e | `rclone lsjson -R --hash pcloud-mcp:pcloud-mcp-test` (74 files, 10 folders, 2.2 MiB) | **12.5 s** (two runs, both 12.5 s); without `--hash` **0.37 s**. 2 `listfolder` + **74 `checksumfile`** calls, done in sequence (~170 ms each). | pCloud listings carry no hashes. Each hash costs one API call. Walks must never ask for hashes. The design's "hash only size collisions, cached in `hashes`, 4 at a time" stands. 4 at a time is ~24 calls/s, so it needs the tps limit. |
| 2 | Does per-call `_filter` exclude a child root? | **Yes.** 0 entries under `Backups/Immich/` in 1b. The `Backups/Immich` folder entry itself is still listed. Its contents aren't fetched (directory pruning). | Confirmed, but not used (item 2 above): a split walk does the same exclusion without losing ListR. The folder entry for a child root should come from the parent's walk, as 1b does. |
| 3 | `--rc-serve` ranged GETs on `pcloud-mcp-test/phase0/range.bin` (1 MiB random) | `bytes=0-99`, `500000-500999`, `1048000-`, `-100`: all **206**, correct `Content-Range` and `Content-Length`, bytes match the local copy (SHA-1), 0.34–0.79 s each. A full GET returns 200 with `Accept-Ranges: bytes` and `Last-Modified`, and matches. `HEAD` works. No auth → 401. **Unsatisfiable** `bytes=2000000-` → **206** with `Content-Range: bytes 2000000-1048575/1048576` and 0 bytes (bug). Multi-range → 400. A folder URL returns an HTML index with `?download=zip` links. **A local path `/[/abs/dir]/file` is served too.** | Confirmed for `/dl` resumes. The core must: (a) clamp and validate `Range` against the stat'ed size and answer 416 itself; (b) build rc-serve URLs only from the fixed remote name plus a `CheckedPath`, percent-encoded per segment; (c) treat `RC_PASS` as full read access to the rclone container's filesystem and env (it already lives only on loopback). rc-serve's built-in `?download=zip` may be worth a look before writing our own zip streamer (untested here, including whether it respects filters). |
| 4 | Filter gap: rcd with `--rc-serve --filter-from=~/.config/pcloud-mcp/filter.txt --filter "- /pcloud-mcp-test/phase0/hidden/**"`; `phase0/hidden/x.txt` created beforehand with plain `rclone copy` | `operations/list` of `phase0` (plain and recursive): **hidden**. `list` of `phase0/hidden` directly: empty. `operations/stat` of `hidden/x.txt` with `showHash`: **returned** (size, ID, MD5, SHA-1). `stat` of the `hidden` folder: returned. rc-serve `GET hidden/x.txt`: **200, contents served**. rc-serve folder index of `phase0`: hidden/ **not shown**. `operations/copyfile` hidden → `phase0/copied-from-hidden.txt`: **succeeded** (the hidden contents now sat in a visible file). `operations/deletefile hidden/x.txt`: **deleted**. `operations/stat "Crypto Folder"`: **found** (`IsDir: true`, ID returned). | Confirmed exactly as the design says: filters only affect listings, and every single-object call goes straight through. `Policy` + `CheckedPath` is the only enforcement. Add `copyfile` to the integration test that proves hidden paths never reach rclone. Note: I also ran one `operations/list` (non-recursive) on "Crypto Folder" through the filtered rcd. It returned 0 entries (filtered). That went beyond the stat-only instruction. Nothing was read or changed. |
| 5 | Trash: `operations/deletefile` on a file, `operations/purge` on a folder (unfiltered rcd) | Both returned 200 `{}`. API: `POST /deletefile` (1 call) and `POST /deletefolderrecursive` (1 call). Source check (v1.75.2 backend/pcloud): `Object.Remove` → `/deletefile`, `Purge` → `/deletefolderrecursive`, `Rmdir` → `/deletefolder`. **There is no `hard_delete` option** in the pCloud backend (unlike some other backends). Permanent deletion exists only as `CleanUp` → `/trash_clear`, which needs username/password (not configured, so `operations/cleanup` would fail on this remote anyway). rclone can't list pCloud's trash (no `showdeleted`), so I couldn't verify it here. | Matches the design (`trash_on_delete: true`), provided pCloud's API puts `deletefile`/`deletefolderrecursive` results in Trash. **Owner: confirm in the pCloud web Trash** (items deleted 2026-10-31 ~14:39–14:41 PDT, all from `pcloud-mcp-test/phase0/`): `p0-trash-file-20261031.txt` (rc `deletefile`); folder `p0-trash-folder-20261031` with `f1.txt`, `f2.txt`, `f3.txt`, `sub/n.txt` (rc `purge`); `hidden/x.txt` (rc `deletefile` through the *filtered* rcd). Cleanup also sent these to the trash: `range.bin`, `copied-from-hidden.txt`, folder `move-dst4` (40 `mNN.txt` + `sub/` with 10 `sNN.txt`). If `p0-trash-folder-20261031` isn't in Trash, `delete` on folders must refuse until the pCloud API backend exists. |
| 6 | `sync/move` of a folder (`phase0/move-src`, 50 files incl. `sub/`) with `deleteEmptySrcDirs: true` | **Unfiltered rcd: ONE `POST /renamefolder`** + 10 `listfolder` (path/dir-cache lookups), log `Server side directory move succeeded`. Same with `_async: true` (job finished, success). **With global `--filter-from`: 50 × `POST /renamefile` + 2 `createfolder` + 1 `deletefolder` + 14 `listfolder`**, and the empty source folder was **left behind**. **With per-call `_filter` only (no global filter): the same per-file behaviour and leftover source folder.** | Confirmed only without filters: rclone's `MoveDir` attempts `DirMove` only when no filter is active. This is a second reason not to run `--filter-from` on the rcd, and the rclone client must never attach `_filter` to `sync/move`/`sync/copy`. Add an integration assertion that a folder move is exactly one `renamefolder`. If any filter is active, a folder move becomes per-file and non-atomic and leaves the source folder behind. |
| 7 | `operations/fsinfo`; `operations/stat` with `showHash` | fsinfo `Hashes: ["md5","sha1"]`, `Precision: 1e9` ns (1 s mtime), `DirMove`, `Purge`, `ListR`, `PublicLink`, `CanHaveEmptyDirectories` all true. stat `showHash` on `range.bin` → `md5` + `sha1`, both equal to the local `md5sum`/`sha1sum`, 0.69 s (one extra `checksumfile` call). `hashTypes: ["sha256"]` → item without hashes (silently unsupported on US). Plain stat 0.33 s. `ID` is pCloud's `f…`/`d…` (e.g. `f95306405495`, `d33742831158`). | Confirmed: US region = SHA-1 + MD5. Entries can take `id` from `ID` directly. Mtime precision is 1 s: compare mtimes at second granularity in the mirror freshness check. |
| 8 | `operations/about` | Works, 0.16 s: `total` 4,398,046,511,104 (4 TiB), `used` 438,398,799,410 (408.3 GiB), `free` 3,959,647,711,694. | Confirmed as the 5-minute health check, and it's cheap. |

## Exact commands

Listing (1a–1c; 1b's tpslimit run added `--tpslimit=8` to `rclone rcd`):

```bash
rclone rcd --rc-addr=127.0.0.1:15572 --rc-user=spike --rc-pass=$PASS [--dump headers] --log-file=…
# 1a
operations/list {"fs":"pcloud-mcp:","remote":"Backups/Immich","opt":{"recurse":true}}
# 1b
operations/list {"fs":"pcloud-mcp:","remote":"","opt":{"recurse":true},"_filter":{"ExcludeRule":["/Backups/Immich/**"]}}
# 1c
operations/list {"fs":"pcloud-mcp:","remote":"","opt":{"recurse":true}}
# 1d: rcd also given --filter-from=/home/ty/.config/pcloud-mcp/filter.txt, then 1a's call
# split walk (rcd --tpslimit=8): list "" (no recurse); for each top-level folder except Backups:
#   list <folder> recurse:true; list "Backups" (no recurse); for each child except Backups/Immich: recurse:true
# 1e
rclone lsjson -R --hash pcloud-mcp:pcloud-mcp-test --dump headers
```

rc-serve (3), filter gap (4), trash (5), moves (6), fsinfo/stat/about (7, 8):

```bash
rclone rcd … --rc-serve
curl -g -u spike:$PASS -H 'Range: bytes=0-99' 'http://127.0.0.1:15572/[pcloud-mcp:]/pcloud-mcp-test/phase0/range.bin'   # also 500000-500999, 1048000-, -100, 2000000-, 0-9,100-109

rclone rcd … --rc-serve --filter-from=/home/ty/.config/pcloud-mcp/filter.txt --filter '- /pcloud-mcp-test/phase0/hidden/**'
operations/list     {"fs":"pcloud-mcp:","remote":"pcloud-mcp-test/phase0","opt":{"recurse":true}}
operations/stat     {"fs":"pcloud-mcp:","remote":"pcloud-mcp-test/phase0/hidden/x.txt","opt":{"showHash":true}}
curl -g -u spike:$PASS 'http://127.0.0.1:15572/[pcloud-mcp:]/pcloud-mcp-test/phase0/hidden/x.txt'
operations/copyfile {"srcFs":"pcloud-mcp:","srcRemote":"pcloud-mcp-test/phase0/hidden/x.txt","dstFs":"pcloud-mcp:","dstRemote":"pcloud-mcp-test/phase0/copied-from-hidden.txt"}
operations/stat     {"fs":"pcloud-mcp:","remote":"Crypto Folder"}
operations/deletefile {"fs":"pcloud-mcp:","remote":"pcloud-mcp-test/phase0/hidden/x.txt"}

operations/deletefile {"fs":"pcloud-mcp:","remote":"pcloud-mcp-test/phase0/p0-trash-file-20261031.txt"}
operations/purge      {"fs":"pcloud-mcp:","remote":"pcloud-mcp-test/phase0/p0-trash-folder-20261031"}

sync/move {"srcFs":"pcloud-mcp:pcloud-mcp-test/phase0/move-src","dstFs":"pcloud-mcp:pcloud-mcp-test/phase0/move-dst","deleteEmptySrcDirs":true}
#   repeated: under rcd --filter-from (→ move-dst2), with "_filter":{"ExcludeRule":["/Crypto Folder/**"]} (→ move-dst3),
#   and unfiltered with "_async":true (→ move-dst4)

operations/fsinfo {"fs":"pcloud-mcp:"}
operations/stat   {"fs":"pcloud-mcp:","remote":"pcloud-mcp-test/phase0/range.bin","opt":{"showHash":true}}
operations/about  {"fs":"pcloud-mcp:"}
```

## Proposed edits to v2.md

- **Backend: rclone rcd.** Remove "kept only as a second layer under listings and the mirror". The rcd runs with **no** `--filter-from`. The rclone client never sends `_filter` on `operations/list` recursions or on `sync/*`. Give the reasons in one line: filters disable ListR (Immich: 6 s → 133 s) and turn `DirMove` into per-file moves.
- **Walks / roots.** Replace "excludes every child root through the per-call `_filter`" with the split walk: list each ancestor of an excluded path (child root or `hidden`) non-recursively, and list every sibling subtree recursively. Measured cost for `/` minus Immich: 12 calls, 4.2 s at `--tpslimit=8`. First full walk: ~6 s for Immich + ~4 s for the rest, not "minutes" (Resources section).
- **Resources.** The rclone container peaks around 520 MB on an unfiltered `/` and 440 MB on Immich. The 1 Gi limit holds if walks per root never overlap. The core receives a 35 MB JSON body for Immich.
- **`--rc-serve`.** The core validates `Range` against the size (rclone answers an unsatisfiable range with a malformed 206). URLs are always `/[<configured remote>:]/<percent-encoded CheckedPath>`. Document that `RC_PASS` also grants local-filesystem reads in the rclone container.
- **Policy tests.** Add `copyfile` (a hidden file copied out to a visible path) to the filter-gap list next to `stat`, `deletefile` and rc-serve `GET`.
- **Capabilities.** `server_dir_move: true` holds only when no filter is active. The live suite asserts one `renamefolder` per folder move.
- **Trash.** Holds pending the owner's look at the pCloud web Trash (names in row 5). Until it's confirmed, phase 3 must not enable folder `delete`.
