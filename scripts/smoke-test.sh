#!/usr/bin/env bash
# Exercises a running MCP endpoint with the MCP Inspector CLI: tools/list, then every tool
# once. Exits non-zero on the first failure.
#
#   scripts/smoke-test.sh                      # the core under `wrangler dev`, dev token
#   scripts/smoke-test.sh URL TOKEN            # any endpoint and bearer token
#
# The defaults match crates/worker/.dev.vars.example and the files in fixtures/webdav-root.
# Against a real pCloud folder, point the tool arguments at files that exist there:
#   SMOKE_TEXT_FILE   file for get_file_info and read_text_file (default /Café menu.txt)
#   SMOKE_IMAGE       picture for read_image (default /images/pixel.png)
#   SMOKE_QUERY       search_files query (default leaf)
#
# The token ends up on npx's command line (the Inspector only takes it as --header), so
# anyone on this machine can see it in `ps` while the test runs. Fine for the dev token; for
# a real one, run this somewhere you trust.
set -euo pipefail

url="${1:-http://127.0.0.1:8787/mcp}"
token="${2:-dev-internal-token}"
text_file="${SMOKE_TEXT_FILE:-/Café menu.txt}"
image="${SMOKE_IMAGE:-/images/pixel.png}"
query="${SMOKE_QUERY:-leaf}"

if ! command -v npx >/dev/null 2>&1; then
  echo "npx not found; install Node.js (or: mise install)" >&2
  exit 1
fi

inspector=(
  npx --yes @modelcontextprotocol/inspector@2.13.0 --cli "$url"
  --transport http
  --header "Authorization: Bearer $token"
  # Fail instead of opening a browser for OAuth when the token is refused.
  --stored-auth-only
  --format json
  --quiet
)

# Reads one Inspector JSON envelope ({"result": ...}) on stdin and prints a one-line summary;
# fails on a tool error or when there is no result.
summarize() {
  node -e '
    let raw = "";
    process.stdin.on("data", (c) => (raw += c));
    process.stdin.on("end", () => {
      let result;
      try {
        result = JSON.parse(raw).result;
      } catch {}
      if (!result) {
        console.log("no result");
        process.exit(1);
      }
      if (Array.isArray(result.tools)) {
        console.log(`${result.tools.length} tools: ${result.tools.map((t) => t.name).join(", ")}`);
        return;
      }
      const parts = (result.content || []).map((c) =>
        c.type === "text" ? c.text : `[${c.type} ${c.mimeType || ""}, ${(c.data || "").length} base64 chars]`,
      );
      const summary = parts.join(" | ").replace(/\s+/g, " ").slice(0, 160);
      if (result.isError) {
        console.log(`tool error: ${summary}`);
        process.exit(1);
      }
      console.log(summary);
    });
  '
}

step() {
  local label="$1"
  shift
  printf '%-16s ' "$label"
  # The Inspector exits non-zero on a tool error too, but still prints the result; show it.
  local out status=0 err
  err="$(mktemp)"
  out="$("${inspector[@]}" "$@" 2>"$err")" || status=$?
  if ! summarize <<<"$out" || ((status != 0)); then
    echo "FAILED (Inspector exit status $status)" >&2
    cat "$err" >&2
    rm -f "$err"
    exit 1
  fi
  rm -f "$err"
}

echo "Smoke-testing $url"
step tools/list --method tools/list
step list_folder --method tools/call --tool-name list_folder --tool-args-json '{"path":"/"}'
step get_file_info --method tools/call --tool-name get_file_info \
  --tool-args-json "$(node -e 'console.log(JSON.stringify({path: process.argv[1]}))' "$text_file")"
step read_text_file --method tools/call --tool-name read_text_file \
  --tool-args-json "$(node -e 'console.log(JSON.stringify({path: process.argv[1], max_bytes: 2000}))' "$text_file")"
step read_image --method tools/call --tool-name read_image \
  --tool-args-json "$(node -e 'console.log(JSON.stringify({path: process.argv[1]}))' "$image")"
step search_files --method tools/call --tool-name search_files \
  --tool-args-json "$(node -e 'console.log(JSON.stringify({query: process.argv[1], limit: 5}))' "$query")"
echo "All tools answered."
