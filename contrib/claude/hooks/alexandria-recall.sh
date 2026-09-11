#!/usr/bin/env bash
# Claude Code UserPromptSubmit hook: auto-recall and heuristic auto-store.
#
# Reads the hook JSON on stdin and, over one MCP (Streamable HTTP) session:
#   1. calls retrieve_memories and emits hits as additionalContext, or a
#      systemMessage warning if the server is unavailable;
#   2. scans the prompt for correction/preference phrasing (same patterns as
#      the Pi extension) and stores unambiguous hits with session_id.
# Always exits 0 so the prompt proceeds either way.
#
# Debug/seed mode: `alexandria-recall.sh <tool> '<json args>'` calls one tool
# and prints its text result.
#
# Env (all optional):
#   ALEXANDRIA_URL                         default http://127.0.0.1:3000/mcp
#   ALEXANDRIA_AUTO_RECALL                 "off" disables recall
#   ALEXANDRIA_AUTO_RECALL_LIMIT           default 10
#   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY  default 0.45
#   ALEXANDRIA_AUTO_STORE                  "off" disables the detectors; "on" enables them in headless sessions (see the entrypoint gate below), where the default is off
#   ALEXANDRIA_MARKER_MAX_AGE_DAYS         default 7; per-session markers idle longer than this are pruned
#   ALEXANDRIA_HOOK_CHILD                  set by hooks that shell out to `claude -p`; exits at once
set -uo pipefail
[ $# -gt 0 ] || input=$(cat)   # before any guard: exiting with stdin unread can SIGPIPE the writer (test.sh pipes jq in)
[ -z "${ALEXANDRIA_HOOK_CHILD:-}" ] || exit 0

URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
# 10 measured on all-MiniLM-L6-v2 by the bench-retrieval limit x threshold grid, 2026-09-09 on
# an 880-fact corpus: delivery saturates at 10 because the worst of the 12 known target ranks is
# 9, so 15 and 20 add non-targets and no hits. The two levers are not independent — read both
# comments together before changing either.
LIMIT="${ALEXANDRIA_AUTO_RECALL_LIMIT:-10}"
# 0.45 from the same grid. At LIMIT=10 it delivers 8 of 12 targets at ~1.0 non-targets per
# prompt, where the previous 5/0.35 pair delivered the same 8 at ~3.2. 0.40 was dominated on
# that grid (same 8 hits, ~2.2 noise) and is a trade on the 20-question one (+2 hits, +1.3
# noise); 0.50 no longer dominates 0.45 the way it did at LIMIT=5 — it
# drops to 7 hits for 0.5 noise. Widening the limit is what moved 0.45 onto the frontier, so do
# not lower LIMIT without revisiting this. See docs/minilm-test-data.md "Result limit".
MIN_SIM="${ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY:-0.45}"
CURL=(curl -sS --max-time 5 -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream')

# post JSON-RPC body; prints the SSE data payload.
post() { "${CURL[@]}" -H "Mcp-Session-Id: $SID" -d "$1" "$URL" | sed -n 's/^data: *//p'; }

mcp_open() { # prints the session id, or an error message with non-zero status
  SID=$("${CURL[@]}" -o /dev/null -w '%header{mcp-session-id}' -d \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"alexandria-recall-hook","version":"1.0"}}}' \
    "$URL" 2>/dev/null) || { echo "cannot reach $URL"; return 1; }
  [ -n "$SID" ] || { echo "no Mcp-Session-Id from $URL"; return 1; }
  post '{"jsonrpc":"2.0","method":"notifications/initialized"}' >/dev/null
  echo "$SID"
}
mcp_tool() { # <tool> <args-json> → tool text result
  local res
  res=$(post "$(jq -cn --arg t "$1" --argjson a "$2" '{jsonrpc:"2.0",id:2,method:"tools/call",params:{name:$t,arguments:$a}}')")
  jq -er '.result.content[] | select(.type=="text") | .text' <<<"$res" 2>/dev/null || { echo "bad response: $res"; return 1; }
}
mcp_close() { "${CURL[@]}" -X DELETE -H "Mcp-Session-Id: $SID" "$URL" >/dev/null 2>&1; }

if [ $# -gt 0 ]; then
  SID=$(mcp_open) || { echo "alexandria-recall: $SID" >&2; exit 1; }
  out=$(mcp_tool "$1" "${2:-{\}}"); rc=$?
  mcp_close
  [ $rc -eq 0 ] || { echo "alexandria-recall: $out" >&2; exit 1; }
  echo "$out"; exit
fi

prompt=$(jq -r '.prompt // ""' <<<"$input" 2>/dev/null) || exit 0
[ -n "${prompt// /}" ] || exit 0
session=$(jq -r '.session_id // ""' <<<"$input")

# ---- heuristic detectors (ported from contrib/pi .../detectors/{correction,preference}.ts)
# Each entry: "<capture group index> <ERE>". \b is spelled (^|[^[:alnum:]_]) and
# counts as a group, hence the explicit index. ERE has no lazy quantifiers; the
# greedy (.+) before "instead of"/"better" differs from Pi only on prompts that
# contain the anchor word twice.
W='(^|[^[:alnum:]_])'; S='[[:space:]]'
CORRECTION=(
  "3 ${W}no[,.]?${S}+(use|it${S}+should${S}+be|it'?s)${S}+(.+)"
  "3 ${W}that'?s${S}+(wrong|incorrect|not${S}+right)[,.]?${S}*(.+)"
  "2 ${W}actually[,.]?${S}+(.+)"
  "2 ${W}i${S}+meant${S}+(.+)"
  "3 ${W}not${S}+.{2,30}[,;]${S}*(use|it'?s)${S}+(.+)"
  "2 ${W}don'?t${S}+use${S}+.{2,30}[,;]${S}*use${S}+(.+)"
  "2 ${W}use${S}+(.+)${S}+instead${S}+of${S}+.+"
  "3 ${W}wrong${S}*(—|–|-)${S}*(.+)"
  "3 ${W}incorrect${S}*(—|–|-)${S}*(.+)"
)
PREFERENCE=(
  "2 ${W}always${S}+(.+)"
  "2 ${W}never${S}+(.+)"
  "2 ${W}i${S}+prefer${S}+(.+)"
  "2 ${W}i${S}+like${S}+(.+)${S}+better"
  "2 ${W}default${S}+to${S}+(.+)"
  "2 ${W}don'?t${S}+ever${S}+(.+)"
  "2 ${W}make${S}+sure${S}+to${S}+(.+)"
  "2 ${W}from${S}+now${S}+on[,.]?${S}+(.+)"
  "2 ${W}going${S}+forward[,.]?${S}+(.+)"
  "2 ${W}use${S}+(.+)${S}+instead${S}+of${S}+(.+)"
)
trim() { local s=$1; [[ $s =~ ^[[:space:]]*(.*[^[:space:].!])[[:space:].!]*$ ]] && s=${BASH_REMATCH[1]}; echo "$s"; }
detect() { # <prefix> <patterns...> → prints "<prefix>: <statement>" for the first match, or nothing
  local prefix=$1 p g stmt; shift
  shopt -s nocasematch
  for p in "$@"; do
    g=${p%% *}; p=${p#* }
    [[ $prompt =~ $p ]] || continue
    stmt=$(trim "${BASH_REMATCH[g]}")
    # Pi's "use X instead of Y" preference keeps both sides.
    [[ $prefix = "User preference" && $g -eq 2 && -n ${BASH_REMATCH[3]:-} && $p == *instead* ]] &&
      stmt="Use $stmt instead of $(trim "${BASH_REMATCH[3]}")"
    [ ${#stmt} -ge 5 ] || continue
    echo "$prefix: $stmt"; return
  done
}
# Sessions with no human at the prompt (`claude -p`, Agent SDK, `claude mcp serve`, bench, GitHub Action,
# triggers) and Cowork carry a CLAUDE_CODE_ENTRYPOINT the binary reserves for them: auto-store is off there
# unless ALEXANDRIA_AUTO_STORE=on, so scripted experiments never land in the real database. Everything else
# (cli, desktop, vscode, the remote family) stays on.
store=${ALEXANDRIA_AUTO_STORE:-}; [ -n "$store" ] || case "${CLAUDE_CODE_ENTRYPOINT:-}" in sdk-*|mcp|bench|claude-code-github-action|claude-security|*_trigger|local-agent|claude-coworker*|remote_cowork) store=off;; esac
detections=()
if [ "$store" != off ] && [ -n "$session" ]; then
  p=$(trim "$prompt")
  if [ ${#p} -ge 8 ] && [ ${#p} -le 500 ]; then
    c=$(detect "User correction" "${CORRECTION[@]}"); [ -n "$c" ] && detections+=("$c")
    c=$(detect "User preference" "${PREFERENCE[@]}"); [ -n "$c" ] && detections+=("$c")
  fi
fi

stored="${XDG_STATE_HOME:-$HOME/.local/state}/alexandria/$session.stored"
# Prune markers idle for over ALEXANDRIA_MARKER_MAX_AGE_DAYS here too, so a machine whose Stop hook
# never fires does not accumulate them (same expression as alexandria-extract.sh).
[ -d "${stored%/*}" ] && find "${stored%/*}" -maxdepth 1 \( -name '*.extracted' -o -name '*.stored' \) -mtime "+${ALEXANDRIA_MARKER_MAX_AGE_DAYS:-7}" -delete

# ---- one MCP session for recall + stores
SID=$(mcp_open) || {
  echo "alexandria-recall: $SID" >&2
  jq -cn --arg m "Alexandria memory unavailable: $SID" '{systemMessage:$m}'
  exit 0
}
trap mcp_close EXIT

for d in "${detections[@]}"; do
  norm=$(tr '[:upper:]' '[:lower:]' <<<"$d" | tr -s '[:space:]' ' ')
  mkdir -p "${stored%/*}"; touch "$stored"
  grep -qxF "$norm" "$stored" && continue
  tag=$([[ $d == "User correction"* ]] && echo correction || echo preference)
  out=$(mcp_tool store_memory "$(jq -cn --arg c "$d" --arg t "$tag" --arg s "$session" '{content:$c,tags:[$t,"auto-detected"],session_id:$s,agent_id:"claude-code"}')") \
    && echo "$norm" >>"$stored" || echo "alexandria-recall: store failed: $out" >&2
done

[ "${ALEXANDRIA_AUTO_RECALL:-}" != "off" ] || exit 0
hits=$(mcp_tool retrieve_memories "$(jq -cn --arg q "$prompt" --argjson l "$LIMIT" '{query:$q,limit:$l}')") || {
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
