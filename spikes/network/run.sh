#!/usr/bin/env bash
# Phase 0 network spike, client side: measures the path workers.dev -> spike Worker -> Workers VPC
# -> tunnel chance-of-files -> spike server, and prints a results table.
#
#   spikes/network/run.sh https://chance-of-files-spike.<sub>.workers.dev/<SPIKE_KEY> [--big] [--quick]
#
#   --big    also download 20 GiB three times in a row (hours on a home uplink)
#   --quick  tiny sizes and short sleeps, to check the script itself (e.g. against a local server)
#
# SPIKE_TOKEN=... adds X-Spike-Token, for talking to the server directly (local run or
# `kubectl port-forward`); through the Worker it isn't needed. Needs curl, sha256sum (or shasum),
# mkfifo and about 100 MiB of free space in $TMPDIR.
set -euo pipefail

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

BASE="" BIG=0 QUICK=0
for arg in "$@"; do
	case "$arg" in
		--big) BIG=1 ;;
		--quick) QUICK=1 ;;
		-h | --help) usage ;;
		-*) echo "unknown option $arg" >&2; usage ;;
		*) BASE="${arg%/}" ;;
	esac
done
[[ -n "$BASE" ]] || usage

GiB=$((1024 * 1024 * 1024)) MiB=$((1024 * 1024))
if ((QUICK)); then
	SLEEPS=(2 3) DL_BYTES=$((64 * MiB)) RESUME_BYTES=$((16 * MiB)) PARTS=3 PART_BYTES=$((5 * MiB)) BIG_BYTES=$((128 * MiB))
else
	SLEEPS=(90 150 200) DL_BYTES=$((2 * GiB)) RESUME_BYTES=$((256 * MiB)) PARTS=22 PART_BYTES=$((90 * MiB)) BIG_BYTES=$((20 * GiB))
fi

if command -v sha256sum >/dev/null; then SHA=(sha256sum); else SHA=(shasum -a 256); fi
CURL=(curl -sS --connect-timeout 20)
[[ -n "${SPIKE_TOKEN:-}" ]] && CURL+=(-H "X-Spike-Token: $SPIKE_TOKEN")

TMP=$(mktemp -d)
trap 'exec 3>&- 2>/dev/null || true; rm -rf "$TMP"' EXIT

ROWS=()
row() { # test, result, detail
	ROWS+=("$1"$'\t'"$2"$'\t'"$3")
	printf '  %-28s %-5s %s\n' "$1" "$2" "$3" >&2
}
hash_of() { "${SHA[@]}" | cut -c1-64; }
json_field() { grep -o "\"$1\":[^,}]*" | head -1 | cut -d: -f2- | tr -d '"'; }
human_rate() { awk -v b="$1" -v t="$2" 'BEGIN { if (t <= 0) t = 1e-9; printf "%.1f MiB/s", b / t / 1048576 }'; }
expected_sha() { "${CURL[@]}" --max-time 600 "$BASE/blob/sha256?bytes=$1" | json_field sha256; }

# Downloads N bytes into sha256sum without touching disk; prints "code bytes seconds sha".
download() {
	local n=$1 w code size time sha
	sha=$("${CURL[@]}" -w '%{stderr}%{http_code} %{size_download} %{time_total}\n' "$BASE/blob?bytes=$n" 2>"$TMP/w" | hash_of) || true
	read -r code size time < <(tail -1 "$TMP/w")
	echo "${code:-000} ${size:-0} ${time:-0} $sha"
}

echo "Spike: $BASE" >&2

# --- MCP smoke test ------------------------------------------------------------------------------
mcp() {
	"${CURL[@]}" --max-time 60 -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
		-d "$1" "$BASE/mcp"
}
init=$(mcp '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"run.sh","version":"0"}}}' || true)
list=$(mcp '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' || true)
held=$(mcp '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"hold","arguments":{"seconds":2}}}' || true)
if [[ "$init" == *protocolVersion* && "$list" == *'"hold"'* && "$held" == *"held 2 s"* ]]; then
	row "mcp initialize/list/hold 2" PASS "tools: hold, blob_url"
else
	row "mcp initialize/list/hold 2" FAIL "${init:0:80} ${held:0:80}"
fi

# --- Quiet responses -----------------------------------------------------------------------------
# In parallel: they don't interact, and sequentially they'd take 7+ minutes.
for s in "${SLEEPS[@]}"; do
	("${CURL[@]}" --max-time $((s + 120)) -o "$TMP/sleep-$s.body" -w '%{http_code} %{time_total}' \
		"$BASE/sleep?s=$s" >"$TMP/sleep-$s.w" 2>"$TMP/sleep-$s.err" || true) &
done
wait
for s in "${SLEEPS[@]}"; do
	read -r code time <"$TMP/sleep-$s.w" || true
	if [[ "${code:-}" == 200 ]] && grep -q "\"slept_s\":$s" "$TMP/sleep-$s.body"; then
		row "plain HTTP quiet ${s} s" PASS "200 after ${time}s"
	else
		row "plain HTTP quiet ${s} s" FAIL "HTTP ${code:-none} after ${time:-?}s $(head -c 100 "$TMP/sleep-$s.body" "$TMP/sleep-$s.err" 2>/dev/null | tr '\n' ' ')"
	fi
done

# --- Download ------------------------------------------------------------------------------------
want=$(expected_sha "$DL_BYTES")
read -r code size time got < <(download "$DL_BYTES")
if [[ "$code" == 200 && "$size" == "$DL_BYTES" && -n "$want" && "$got" == "$want" ]]; then
	row "download $((DL_BYTES / MiB)) MiB" PASS "sha256 ok, $(human_rate "$size" "$time") in ${time}s"
else
	row "download $((DL_BYTES / MiB)) MiB" FAIL "HTTP $code, $size bytes, sha ${got:0:12} want ${want:0:12}"
fi

# --- Resume with Range ---------------------------------------------------------------------------
want=$(expected_sha "$RESUME_BYTES")
f="$TMP/resume.bin"
"${CURL[@]}" -r "0-$((RESUME_BYTES / 3 - 1))" -o "$f" "$BASE/blob?bytes=$RESUME_BYTES" || true
first=$(wc -c <"$f" | tr -d ' ')
code=$("${CURL[@]}" -C - -o "$f" -w '%{http_code}' "$BASE/blob?bytes=$RESUME_BYTES" || true)
got=$(hash_of <"$f")
if [[ "$code" == 206 && "$got" == "$want" ]]; then
	row "resume $((RESUME_BYTES / MiB)) MiB with Range" PASS "first $first bytes, then 206 for the rest; sha256 ok"
else
	row "resume $((RESUME_BYTES / MiB)) MiB with Range" FAIL "second request HTTP $code, sha ${got:0:12} want ${want:0:12}"
fi
rm -f "$f"

# --- Chunked upload ------------------------------------------------------------------------------
# Random parts, one at a time on disk; a FIFO feeds every part into one sha256sum for the whole.
id="run-$(date +%s)-$$"
mkfifo "$TMP/whole.fifo"
hash_of <"$TMP/whole.fifo" >"$TMP/whole.sha" &
hasher=$!
exec 3>"$TMP/whole.fifo"
up_ok=1 up_time=0 up_bytes=0 slowest=0
for ((k = 0; k < PARTS; k++)); do
	part="$TMP/part"
	head -c "$PART_BYTES" /dev/urandom >"$part"
	local_sha=$(hash_of <"$part")
	cat "$part" >&3
	res=$("${CURL[@]}" --max-time 900 -T "$part" -H 'Content-Type: application/octet-stream' \
		-w '\n%{http_code} %{time_total}' "$BASE/chunk?upload=$id&part=$k" || true)
	read -r code time <<<"$(tail -1 <<<"$res")"
	if [[ "$code" != 200 || "$(json_field sha256 <<<"$res")" != "$local_sha" ]]; then
		up_ok=0
		echo "  part $k: HTTP $code $(head -1 <<<"$res" | head -c 120)" >&2
	fi
	up_time=$(awk -v a="$up_time" -v b="$time" 'BEGIN { print a + b }')
	slowest=$(awk -v a="$slowest" -v b="$time" 'BEGIN { print (b > a ? b : a) }')
	up_bytes=$((up_bytes + PART_BYTES))
	printf '  part %2d/%d: HTTP %s in %ss\n' "$((k + 1))" "$PARTS" "$code" "$time" >&2
done
rm -f "$TMP/part"
exec 3>&-
wait "$hasher"
fin=$("${CURL[@]}" -X POST "$BASE/finish?upload=$id" || true)
whole=$(cat "$TMP/whole.sha")
if ((up_ok)) && [[ "$(json_field bytes <<<"$fin")" == "$up_bytes" && "$(json_field sha256 <<<"$fin")" == "$whole" ]]; then
	row "upload ${PARTS} x $((PART_BYTES / MiB)) MiB" PASS "every part and whole sha256 ok, $(human_rate "$up_bytes" "$up_time") (slowest part ${slowest}s)"
else
	row "upload ${PARTS} x $((PART_BYTES / MiB)) MiB" FAIL "finish: ${fin:0:160}"
fi

# --- Big downloads -------------------------------------------------------------------------------
if ((BIG)); then
	want=$(expected_sha "$BIG_BYTES")
	for i in 1 2 3; do
		read -r code size time got < <(download "$BIG_BYTES")
		if [[ "$code" == 200 && "$size" == "$BIG_BYTES" && -n "$want" && "$got" == "$want" ]]; then
			row "download $((BIG_BYTES / MiB)) MiB #$i" PASS "sha256 ok, $(human_rate "$size" "$time") in ${time}s"
		else
			row "download $((BIG_BYTES / MiB)) MiB #$i" FAIL "HTTP $code, $size of $BIG_BYTES bytes after ${time}s"
		fi
	done
fi

# --- Table ---------------------------------------------------------------------------------------
echo
printf '%-30s %-6s %s\n' TEST RESULT DETAIL
printf '%-30s %-6s %s\n' "------------------------------" ------ ------------------------------
fails=0
for r in "${ROWS[@]}"; do
	IFS=$'\t' read -r t res d <<<"$r"
	printf '%-30s %-6s %s\n' "$t" "$res" "$d"
	[[ "$res" == PASS ]] || fails=$((fails + 1))
done
exit $((fails > 0))
