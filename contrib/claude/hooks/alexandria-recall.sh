#!/usr/bin/env bash
# Claude Code UserPromptSubmit hook: authenticated, Personal-only or explicitly project-scoped recall.
# Automatic prompt-regex stores were removed. Save durable observations through the agent's explicit
# authenticated store_memory tool and the Alexandria memory skill.
#
# Debug mode: `alexandria-recall.sh <tool> '<json args>'` calls one authenticated tool.
#
# Env (all optional):
#   ALEXANDRIA_URL                         default http://127.0.0.1:3000/mcp
#   ALEXANDRIA_API_TOKEN                   bearer token for this hook's HTTP client
#   ALEXANDRIA_PROJECT_ID                  explicit existing owned project handle; resolved before recall
#   ALEXANDRIA_AUTO_RECALL                 "off" disables recall
#   ALEXANDRIA_AUTO_RECALL_LIMIT           default 10
#   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY  default 0.45
#   ALEXANDRIA_HOOK_CHILD                  set by hooks that shell out to `claude -p`; exits at once
set -uo pipefail
[ $# -gt 0 ] || input=$(cat)
[ -z "${ALEXANDRIA_HOOK_CHILD:-}" ] || exit 0

URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
LIMIT="${ALEXANDRIA_AUTO_RECALL_LIMIT:-10}"
MIN_SIM="${ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY:-0.45}"

hcurl() {
  local left=$((8 - SECONDS)) token=${ALEXANDRIA_API_TOKEN:-}
  if [[ $token == *$'\r'* || $token == *$'\n'* ]]; then
    return 1
  fi
  # Request bodies are passed as arguments, leaving stdin free for the secret header.
  printf 'Authorization: Bearer %s\n' "$token" | ALEXANDRIA_API_TOKEN= \
    curl -sS --max-time "$((left > 5 ? 5 : left < 1 ? 1 : left))" \
      -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
      -H @- "$@"
}
post() {
  local body=$1
  [ -n "$body" ] || { echo "Alexandria: refusing to POST an empty MCP request body." >&2; return 1; }
  hcurl -H "Mcp-Session-Id: $SID" --data-binary "$body" "$URL" | sed -n 's/^data: *//p'
}
mcp_open() {
  [ -n "${ALEXANDRIA_API_TOKEN:-}" ] || { echo "Configure ALEXANDRIA_API_TOKEN for authenticated Alexandria hooks."; return 1; }
  SID=$(hcurl -o /dev/null -w '%header{mcp-session-id}' -d \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"alexandria-recall-hook","version":"2.0"}}}' \
    "$URL" 2>/dev/null) || { echo "cannot reach authenticated Alexandria server"; return 1; }
  [ -n "$SID" ] || { echo "no Mcp-Session-Id from $URL"; return 1; }
  post '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null
}
mcp_tool() {
  local tool_name=$1
  local args_json=$2
  local request_body response
  request_body=$(jq -cn --arg t "$tool_name" --argjson a "$args_json" \
    '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:$t,arguments:$a}}') || return 1
  [ -n "$request_body" ] || { echo "Alexandria: failed to encode the MCP request body." >&2; return 1; }
  response=$(post "$request_body") || return 1
  jq -er 'if .result.structuredContent != null then .result.structuredContent | tojson else .result.content[] | select(.type=="text") | .text end' <<<"$response" 2>/dev/null || { echo "bad response: $response"; return 1; }
}
mcp_close() { hcurl -X DELETE -H "Mcp-Session-Id: $SID" "$URL" >/dev/null 2>&1; }

if [ $# -gt 0 ]; then
  tool_name=$1
  tool_args=${2:-'{}'}
  SID=""
  mcp_open || { echo "alexandria-recall: $SID" >&2; exit 1; }
  out=$(mcp_tool "$tool_name" "$tool_args"); rc=$?
  mcp_close
  [ $rc -eq 0 ] || { echo "alexandria-recall: $out" >&2; exit 1; }
  echo "$out"
  exit
fi

prompt=$(jq -r '.prompt // ""' <<<"$input" 2>/dev/null) || exit 0
[ -n "${prompt// /}" ] || exit 0
[ "${ALEXANDRIA_AUTO_RECALL:-}" != "off" ] || exit 0

SID=""
if ! mcp_open; then
  jq -cn --arg m "Alexandria memory unavailable: ${SID:-configure ALEXANDRIA_API_TOKEN and verify server authentication}" '{systemMessage:$m}'
  exit 0
fi
trap mcp_close EXIT

if [ -n "${ALEXANDRIA_PROJECT_ID:-}" ]; then
  resolved=$(mcp_tool resolve_project "$(jq -cn --arg p "$ALEXANDRIA_PROJECT_ID" '{project:$p}')") || {
    jq -cn --arg m "Alexandria project context could not be resolved; refusing an unscoped recall." '{systemMessage:$m}'
    exit 0
  }
  project=$(jq -er '.project.id | strings | select(startswith("project:"))' <<<"$resolved") || {
    jq -cn '{systemMessage:"Alexandria returned no issued project ID; refusing an unscoped recall."}'
    exit 0
  }
  args=$(jq -cn --arg q "$prompt" --argjson l "$LIMIT" --arg p "$project" '{query:$q,limit:$l,scope:"auto",project:$p}')
else
  # Without an explicit owned project ID this hook stays Personal-only. It deliberately does not
  # implement a second Git-remote normalizer alongside the Pi companion's shared contract.
  args=$(jq -cn --arg q "$prompt" --argjson l "$LIMIT" '{query:$q,limit:$l,scope:"personal"}')
fi
hits=$(mcp_tool retrieve_memories "$args") || {
  jq -cn --arg m "Alexandria memory unavailable: $hits" '{systemMessage:$m}'
  exit 0
}
lines=$(jq -r --argjson m "$MIN_SIM" '.results[]? | select(.similarity >= $m)
  | "- (similarity \(.similarity*100|round/100), id \(.id))\(.tags|if length>0 then " [\(join(", "))]" else "" end) \(.content)"' <<<"$hits")
[ -n "$lines" ] || exit 0
ctx="Relevant memories retrieved automatically from Alexandria for this prompt:
$lines

These are surfaced proactively; verify relevance before relying on them, and use update_memory if any is stale."
jq -cn --arg c "$ctx" '{hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:$c}}'
