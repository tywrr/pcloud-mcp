# Phase 0: network spikes

**Date:** 2026-10-31. **Design:** [v2.md](v2.md), phase 0 ("Spikes"), network part. The rclone part is in [phase0-rclone.md](phase0-rclone.md). The spike itself (server, Worker, manifests, `run.sh`) is in [spikes/network](../../spikes/network/README.md).
**Path measured:** client → Worker `chance-of-files-spike` on workers.dev (Workers **free** plan) → Workers VPC service `chance-of-files` → tunnel `chance-of-files` → cloudflared (QUIC) in namespace `chance-of-files-spike` on the cm3588 → spike server on `127.0.0.1:8080`. This is the path v2 uses, except that the spike Worker stands in for the auth Worker.

## Summary of what changes in the design

1. **`server.deadline` = 180 s** (was 80 s). Quiet responses held 90, 150 and 200 s all came back, both as plain HTTP and as MCP tool calls from claude.ai. 200 s minus the 20 s margin is 180 s, under Claude's own 240 s cap.
2. **Big transfers work through the free Worker.** 2 GiB down (laptop and Duet) and 2 GiB up in 90 MiB parts arrived intact, and `Range` resume works. Speed is set by the home uplink, not by Cloudflare.
3. **D4 is settled by measurement: exact-IP binding would break.** claude.ai's MCP calls came from about 15 different addresses in one /24, and that /24 is Anthropic's shared egress, also used by Duet. Binding to the first /24 stays as a minor layer only; see v2's link security section.

## Results

| # | Question | Result | Meaning for the design |
|---|---|---|---|
| 1 | Quiet plain-HTTP response (`GET /sleep?s=N`) held 90 / 150 / 200 s | **All three 200**, body intact | Nothing on the path cuts a quiet response at ~100 s. |
| 2 | MCP tool call `hold` from claude.ai, 90 / 150 / 200 s (new message each) | **All three completed**, "held N s" returned | `server.deadline` = 200 − 20 = **180 s**. Claude's cap is 240 s. |
| 3 | 2 GiB download from the laptop (`run.sh`) | **Intact**, **18.3 MiB/s**, 111.8 s | Single-file links through workers.dev work at uplink speed. |
| 4 | `Range` resume of the 2 GiB download | **Works** | `curl -C -` resumes stand as designed. |
| 5 | 2 GiB upload as 22 × 90 MiB `PUT /chunk` parts | **Intact**, **10.9 MiB/s** | The 100 MB body cap is no obstacle for 90 MiB parts; chunked uploads stand. |
| 6 | Duet: add the workers.dev host to the network allowlist, `blob_url` 2 GiB, `curl` + `sha256sum` on Duet's computer | **Intact**: 2,147,483,648 bytes in **104.6 s, ~20.5 MB/s**. Worked once the allowlist had the workers.dev host (host only, no key or path) | Duet reaches links on workers.dev; one allowlisted host is enough. |
| 7 | Duet's egress address during the download | Every `/blob` request came from **160.79.106.131** | Stable for one transfer. |
| 8 | claude.ai's MCP call addresses (client IPs in the spike server log) | **~15 different addresses, all in 160.79.106.0/24**, within Anthropic's shared egress range 160.79.104.0/21 | Exact-IP binding would break as soon as a link is used from a different call or machine. A /24 binding keeps out the open internet but not other Claude users who share Anthropic's egress (D4, below). |
| 9 | 3 × 20 GiB downloads in a row (`run.sh --big`) | **Skipped**: hours of the home uplink | Remains a residual risk in v2 ("Long transfers through the free Worker"). None of the 2 GiB transfers (up to ~3 min each) was cut off. |

## D4: link IP binding

The question was whether to bind a link to the first client's exact IP, its /24 (/64 for IPv6), or nothing, and to tighten to exact IP if Duet's egress proved stable.

- Duet's 2 GiB download came from a single address, 160.79.106.131.
- claude.ai's MCP calls, though, came from about 15 addresses in 160.79.106.0/24. Calls and downloads from Claude's side don't come from one fixed address, so exact-IP binding would 403 legitimate retries and resumes.
- The whole /24 sits inside 160.79.104.0/21, which Anthropic uses as shared egress. Any other Claude or Duet user's traffic can leave from the same /24. So a /24 binding stops someone on the open internet who gets hold of a URL, but not a request routed through another Claude user's session.

**Outcome:** keep binding to the first client's /24 (/64 for IPv6) as a minor layer, and don't present it as the main protection. The real protections are the 15-minute default TTL, the per-link byte cap, the `destructiveHint` on `create_download_link` (Claude asks before making a link), `list_links` / `revoke_link`, the per-use log, and the unguessable HMAC'd URL. v2's link section says this.

## How it was run

`spikes/network/run.sh "$BASE"` from the laptop for rows 1, 3, 4 and 5; the owner in claude.ai (custom connector, no sign-in) for row 2; the owner in Duet for rows 6 and 7; client IPs from `kubectl -n chance-of-files-spike logs deploy/network-spike -c spike` for rows 7 and 8. Setup, commands and cleanup are in [spikes/network/README.md](../../spikes/network/README.md).
