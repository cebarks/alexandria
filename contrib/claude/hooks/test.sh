#!/usr/bin/env bash
# Manual end-to-end check for alexandria-recall.sh against a running server.
# Usage: ALEXANDRIA_URL=http://127.0.0.1:3000/mcp ./test.sh
set -euo pipefail
cd "$(dirname "$0")"
export ALEXANDRIA_URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
export ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY=0.0

fact="The hook test project uses SurrealDB as its database backend"
# Seed one memory using the script's own MCP helper.
./alexandria-recall.sh store_memory \
  "$(jq -cn --arg c "$fact" '{content:$c,tags:["hook-test"]}')" >/dev/null

out=$(jq -cn '{session_id:"sess-test-123",prompt:"which database does the hook test project use"}' | ./alexandria-recall.sh)
echo "$out"
grep -q 'session_id for this conversation: sess-test-123' <<<"$out"
grep -q 'SurrealDB' <<<"$out"

# Empty prompt prints nothing.
[ -z "$(jq -cn '{session_id:"x",prompt:""}' | ./alexandria-recall.sh)" ]
# Unreachable server fails open: exit 0, empty stdout.
[ -z "$(jq -cn '{session_id:"x",prompt:"hi"}' | ALEXANDRIA_URL=http://127.0.0.1:1/mcp ./alexandria-recall.sh 2>/dev/null)" ]
echo OK
