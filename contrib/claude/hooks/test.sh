#!/usr/bin/env bash
# Manual end-to-end check for the Claude Code hooks against a running server.
# Usage: ALEXANDRIA_URL=http://127.0.0.1:3000/mcp ./test.sh
set -euo pipefail
cd "$(dirname "$0")"
export ALEXANDRIA_URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
export ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY=0.0

fact="The hook test project uses SurrealDB as its database backend"
# Seed one memory using the script's own MCP helper.
./alexandria-recall.sh store_memory \
  "$(jq -cn --arg c "$fact" '{content:$c,tags:["hook-test"]}')" >/dev/null

# Recall: hit lands in additionalContext.
out=$(jq -cn '{session_id:"sess-test-123",prompt:"which database does the hook test project use"}' | ./alexandria-recall.sh)
echo "$out"
jq -e '.hookSpecificOutput.additionalContext | test("SurrealDB")' <<<"$out" >/dev/null

# Empty prompt prints nothing.
[ -z "$(jq -cn '{session_id:"x",prompt:""}' | ./alexandria-recall.sh)" ]
# Unreachable server fails open: exit 0, systemMessage warning.
out=$(jq -cn '{session_id:"x",prompt:"hi"}' | ALEXANDRIA_URL=http://127.0.0.1:1/mcp ./alexandria-recall.sh 2>/dev/null)
jq -e '.systemMessage | test("unavailable")' <<<"$out" >/dev/null
# Child guard: no output, no network.
[ -z "$(jq -cn '{prompt:"hi"}' | ALEXANDRIA_HOOK_CHILD=1 ALEXANDRIA_URL=http://127.0.0.1:1/mcp ./alexandria-recall.sh)" ]

# Session hook: injects session_id when missing, silent when present.
out=$(jq -cn '{session_id:"sess-test-123",tool_name:"mcp__alexandria__store_memory",tool_input:{content:"x"}}' | ./alexandria-session.sh)
[ "$(jq -r '.hookSpecificOutput.updatedInput.session_id' <<<"$out")" = "sess-test-123" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.content' <<<"$out")" = "x" ]
[ -z "$(jq -cn '{session_id:"s",tool_input:{content:"x",session_id:"already"}}' | ./alexandria-session.sh)" ]
# Garbage stdin never blocks the tool call.
[ -z "$(echo 'not json' | ./alexandria-session.sh)" ]
echo OK
