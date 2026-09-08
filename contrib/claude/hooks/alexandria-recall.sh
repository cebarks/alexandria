#!/usr/bin/env bash
# Claude Code UserPromptSubmit hook: auto-recall from Alexandria.
#
# Reads the hook JSON on stdin, calls retrieve_memories over MCP (Streamable
# HTTP), and emits hook JSON: matching memories as additionalContext, or a
# systemMessage warning if the server is unavailable. Always exits 0 so the
# prompt proceeds either way.
#
# Debug/seed mode: `alexandria-recall.sh <tool> '<json args>'` calls one tool
# and prints its text result.
#
# Env (all optional):
#   ALEXANDRIA_URL                         default http://127.0.0.1:3000/mcp
#   ALEXANDRIA_AUTO_RECALL                 "off" disables
#   ALEXANDRIA_AUTO_RECALL_LIMIT           default 5
#   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY  default 0.35
#   ALEXANDRIA_HOOK_CHILD                  set by hooks that shell out to `claude -p`; exits at once
set -uo pipefail
[ -z "${ALEXANDRIA_HOOK_CHILD:-}" ] || exit 0

URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
LIMIT="${ALEXANDRIA_AUTO_RECALL_LIMIT:-5}"
# 0.35 measured 2026-09-08 on all-MiniLM-L6-v2: question-vs-matching-statement
# scores 0.40-0.65, unrelated memories 0.07-0.40. See docs/configuration.md [recall].
MIN_SIM="${ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY:-0.35}"
CURL=(curl -sS --max-time 5 -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')

# Error path: message on stdout, non-zero status. Callers decide where it goes.
fail() { echo "$*"; exit 1; }

# post JSON-RPC body; prints the SSE data payload.
post() { "${CURL[@]}" -H "Mcp-Session-Id: $SID" -d "$1" "$URL" | sed -n 's/^data: *//p'; }

mcp_call() { # <tool> <args-json> → tool text result
  SID=$("${CURL[@]}" -o /dev/null -w '%header{mcp-session-id}' -d \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"alexandria-recall-hook","version":"1.0"}}}' \
    "$URL" 2>/dev/null) || fail "cannot reach $URL"
  [ -n "$SID" ] || fail "no Mcp-Session-Id from $URL"
  post '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null
  local res
  res=$(post "$(jq -cn --arg t "$1" --argjson a "$2" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:$t,arguments:$a}}')")
  "${CURL[@]}" -X DELETE -H "Mcp-Session-Id: $SID" "$URL" >/dev/null 2>&1
  jq -er '.result.content[] | select(.type=="text") | .text' <<<"$res" 2>/dev/null || fail "bad response: $res"
}

if [ $# -gt 0 ]; then
  out=$(mcp_call "$1" "${2:-{\}}") || { echo "alexandria-recall: $out" >&2; exit 1; }
  echo "$out"; exit
fi

[ "${ALEXANDRIA_AUTO_RECALL:-}" != "off" ] || exit 0
prompt=$(jq -r '.prompt // ""' 2>/dev/null) || exit 0
[ -n "${prompt// /}" ] || exit 0

hits=$(mcp_call retrieve_memories "$(jq -cn --arg q "$prompt" --argjson l "$LIMIT" '{query:$q,limit:$l}')") || {
  echo "alexandria-recall: $hits" >&2
  jq -cn --arg m "Alexandria memory unavailable: $hits" '{systemMessage:$m}'
  exit 0
}
lines=$(jq -r --argjson m "$MIN_SIM" '.results[]? | select(.similarity >= $m)
  | "- (similarity \(.similarity*100|round/100), id \(.id))\(if (.tags|length)>0 then " [\(.tags|join(", "))]" else "" end) \(.content)"' <<<"$hits")
[ -n "$lines" ] || exit 0

ctx="Relevant memories retrieved automatically from Alexandria for this prompt:
$lines

These are surfaced proactively; verify relevance before relying on them, and use update_memory if any is stale."
jq -cn --arg c "$ctx" '{hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:$c}}'
