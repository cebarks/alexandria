#!/usr/bin/env bash
# Manual integration smoke test for authenticated Claude recall and explicit stores.
# Usage: ALEXANDRIA_API_TOKEN=... ALEXANDRIA_URL=http://127.0.0.1:3000/mcp ./alexandria-claude-test.sh
set -euo pipefail
cd "$(dirname "$0")"
[ -n "${ALEXANDRIA_API_TOKEN:-}" ] || { echo "Set ALEXANDRIA_API_TOKEN for the authenticated test." >&2; exit 2; }
export ALEXANDRIA_URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
export ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY=0.0
unset CLAUDE_CODE_ENTRYPOINT
XDG_STATE_HOME=$(mktemp -d); export XDG_STATE_HOME
sess="sess-test-$$"
cleanup() {
  ./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" 2>/dev/null \
    | jq -r '.memories[]?.id' | while read -r id; do
      ./alexandria-recall.sh delete_memory "$(jq -cn --arg i "$id" '{id:$i}')" >/dev/null
    done
}
trap 'cleanup; rm -rf "$XDG_STATE_HOME"' EXIT

fact="The hook test project uses SurrealDB as its database backend"
./alexandria-recall.sh store_memory \
  "$(jq -cn --arg c "$fact" --arg s "$sess" '{content:$c,tags:["hook-test"],scope:"personal",session_id:$s,agent_id:"claude-code"}')" >/dev/null

out=$(jq -cn '{session_id:"sess-test-123",prompt:"which database does the hook test project use"}' | ./alexandria-recall.sh)
jq -e '.hookSpecificOutput.additionalContext | test("SurrealDB")' <<<"$out" >/dev/null
[ -z "$(jq -cn '{session_id:"x",prompt:""}' | ./alexandria-recall.sh)" ]
out=$(jq -cn '{session_id:"x",prompt:"hi"}' | ALEXANDRIA_URL=http://127.0.0.1:1/mcp ./alexandria-recall.sh 2>/dev/null)
jq -e '.systemMessage | test("unavailable")' <<<"$out" >/dev/null
[ -z "$(jq -cn '{prompt:"hi"}' | ALEXANDRIA_HOOK_CHILD=1 ALEXANDRIA_URL=http://127.0.0.1:1/mcp ./alexandria-recall.sh)" ]

# The only store above is an explicit authenticated tool call. Correction/preference-shaped prompts
# remain recall-only; the hook has no Stop-hook or regex-triggered writer.
out=$(jq -cn --arg s "$sess" --arg p "No, use jj instead of git; always run clippy first" \
  '{session_id:$s,prompt:$p}' | ./alexandria-recall.sh)
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" \
  | jq '[.memories[] | select(.tags|index("auto-detected"))] | length')" = 0 ]
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" \
  | jq '[.memories[] | select(.tags|index("extracted"))] | length')" = 0 ]

# Session attribution preserves explicit scope and project context instead of rewriting the target.
out=$(jq -cn '{session_id:"sess-test-123",tool_name:"mcp__alexandria__store_memory",tool_input:{content:"x",scope:"personal",context_project:"project:owned"}}' | ./alexandria-session.sh)
[ "$(jq -r '.hookSpecificOutput.updatedInput.session_id' <<<"$out")" = "sess-test-123" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.agent_id' <<<"$out")" = "claude-code" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.scope' <<<"$out")" = "personal" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.context_project' <<<"$out")" = "project:owned" ]
[ -z "$(jq -cn '{session_id:"s",tool_input:{content:"x",session_id:"already",agent_id:"other"}}' | ./alexandria-session.sh)" ]
echo OK
