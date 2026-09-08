#!/usr/bin/env bash
# Claude Code UserPromptSubmit hook: auto-recall from Alexandria.
#
# Reads the hook JSON on stdin, calls retrieve_memories over MCP (Streamable
# HTTP), prints matching memories plus the Claude Code session_id to stdout so
# they land in context. Fails open: any error exits 0 with empty stdout.
#
# Debug/seed mode: `alexandria-recall.sh <tool> '<json args>'` calls one tool
# and prints its text result.
#
# Env (all optional):
#   ALEXANDRIA_URL                         default http://127.0.0.1:3000/mcp
#   ALEXANDRIA_AUTO_RECALL                 "off" disables
#   ALEXANDRIA_AUTO_RECALL_LIMIT           default 5
#   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY  default 0.15
set -uo pipefail

URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
LIMIT="${ALEXANDRIA_AUTO_RECALL_LIMIT:-5}"
# ponytail: 0.15 is a guess (question-vs-statement scores ~0.19 on MiniLM, server floor 0.10);
# re-measure per TODO-misc.md "Auto-recall client default" and adjust.
MIN_SIM="${ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY:-0.15}"
CURL=(curl -sS --max-time 5 -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')

fail() { echo "alexandria-recall: $*" >&2; exit 1; }

# post JSON-RPC body; prints the SSE data payload.
post() { "${CURL[@]}" -H "Mcp-Session-Id: $SID" -d "$1" "$URL" | sed -n 's/^data: *//p'; }

mcp_call() { # <tool> <args-json> → tool text result
  SID=$("${CURL[@]}" -o /dev/null -w '%header{mcp-session-id}' -d \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"alexandria-recall-hook","version":"1.0"}}}' \
    "$URL") || fail "cannot reach $URL"
  [ -n "$SID" ] || fail "no Mcp-Session-Id from $URL"
  post '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null
  local res
  res=$(post "$(jq -cn --arg t "$1" --argjson a "$2" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:$t,arguments:$a}}')")
  "${CURL[@]}" -X DELETE -H "Mcp-Session-Id: $SID" "$URL" >/dev/null 2>&1
  jq -er '.result.content[] | select(.type=="text") | .text' <<<"$res" 2>/dev/null || fail "bad response: $res"
}

if [ $# -gt 0 ]; then mcp_call "$1" "${2:-{\}}"; exit; fi

[ "${ALEXANDRIA_AUTO_RECALL:-}" != "off" ] || exit 0
input=$(cat)
prompt=$(jq -r '.prompt // ""' <<<"$input")
session=$(jq -r '.session_id // ""' <<<"$input")
[ -n "${prompt// /}" ] || exit 0

hits=$(mcp_call retrieve_memories "$(jq -cn --arg q "$prompt" --argjson l "$LIMIT" '{query:$q,limit:$l}')") || exit 0
lines=$(jq -r --argjson m "$MIN_SIM" '.results[]? | select(.similarity >= $m)
  | "- (similarity \(.similarity*100|round/100), id \(.id))\(if (.tags|length)>0 then " [\(.tags|join(", "))]" else "" end) \(.content)"' <<<"$hits")

if [ -n "$lines" ]; then
  echo "Relevant memories retrieved automatically from Alexandria for this prompt:"
  echo "$lines"
  echo
  echo "These are surfaced proactively; verify relevance before relying on them, and use update_memory if any is stale."
fi
[ -z "$session" ] || echo "Alexandria session_id for this conversation: $session. Pass it as session_id to store_memory."
