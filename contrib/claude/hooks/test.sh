#!/usr/bin/env bash
# Manual end-to-end check for the Claude Code hooks against a running server.
# Usage: ALEXANDRIA_URL=http://127.0.0.1:3000/mcp ./test.sh
set -euo pipefail
cd "$(dirname "$0")"
export ALEXANDRIA_URL="${ALEXANDRIA_URL:-http://127.0.0.1:3000/mcp}"
export ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY=0.0
unset CLAUDE_CODE_ENTRYPOINT ALEXANDRIA_AUTO_STORE   # hermetic: the harness running this script may be headless
XDG_RUNTIME_DIR=$(mktemp -d); export XDG_RUNTIME_DIR
trap 'rm -rf "$XDG_RUNTIME_DIR"' EXIT
sess="sess-test-$$"
cleanup() { # delete every memory stored under $sess so test runs don't pile up
  ./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" 2>/dev/null \
    | jq -r '.memories[]?.id' | while read -r id; do
      ./alexandria-recall.sh delete_memory "$(jq -cn --arg i "$id" '{id:$i}')" >/dev/null; done
}
trap 'cleanup; rm -rf "$XDG_RUNTIME_DIR"' EXIT

fact="The hook test project uses SurrealDB as its database backend"
# Seed one memory using the script's own MCP helper.
./alexandria-recall.sh store_memory \
  "$(jq -cn --arg c "$fact" --arg s "$sess" '{content:$c,tags:["hook-test"],session_id:$s}')" >/dev/null

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

# Detectors: correction + preference stored with session_id, deduped per session.
hook() { jq -cn --arg s "$sess" --arg p "$1" '{session_id:$s,prompt:$p}' | ./alexandria-recall.sh >/dev/null; }
hook "no, use jj instead of git"
hook "always run clippy before pushing"
hook "no, use jj instead of git"
got=$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq -c '[.memories[] | select(.tags|index("auto-detected")) | .content] | sort')
echo "$got"
[ "$got" = '["User correction: jj instead of git","User preference: Use jj instead of git","User preference: run clippy before pushing"]' ]
# Auto-store off: nothing new.
ALEXANDRIA_AUTO_STORE=off hook "never use tabs"
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("auto-detected"))] | length')" = 3 ]
# Headless session (CLAUDE_CODE_ENTRYPOINT=sdk-*): detectors off by default, on with ALEXANDRIA_AUTO_STORE=on.
CLAUDE_CODE_ENTRYPOINT=sdk-cli hook "never use spaces"
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("auto-detected"))] | length')" = 3 ]
CLAUDE_CODE_ENTRYPOINT=sdk-cli ALEXANDRIA_AUTO_STORE=on hook "never use spaces"
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("auto-detected"))] | length')" = 4 ]

# Session hook: injects session_id when missing, silent when present.
out=$(jq -cn '{session_id:"sess-test-123",tool_name:"mcp__alexandria__store_memory",tool_input:{content:"x"}}' | ./alexandria-session.sh)
[ "$(jq -r '.hookSpecificOutput.updatedInput.session_id' <<<"$out")" = "sess-test-123" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.content' <<<"$out")" = "x" ]
[ -z "$(jq -cn '{session_id:"s",tool_input:{content:"x",session_id:"already"}}' | ./alexandria-session.sh)" ]
# Garbage stdin never blocks the tool call.
[ -z "$(echo 'not json' | ./alexandria-session.sh)" ]
# Session hook is generic over tool_input: import_document payload gets the same treatment.
out=$(jq -cn '{session_id:"sess-test-123",tool_name:"mcp__alexandria__import_document",tool_input:{content:"doc text",mode:"chunk"}}' | ./alexandria-session.sh)
[ "$(jq -r '.hookSpecificOutput.updatedInput.session_id' <<<"$out")" = "sess-test-123" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.mode' <<<"$out")" = "chunk" ]
# Extract hook: fake transcript + stub LLM, incremental marker, extracted tag.
td=$(mktemp -d); trap 'cleanup; rm -rf "$XDG_RUNTIME_DIR" "$td"' EXIT
jq -cn '{type:"user",message:{content:"<local-command-caveat>ignore me</local-command-caveat>"}}
        ,{type:"user",message:{content:"which storage engine should we pick?"}}
        ,{type:"assistant",message:{content:[{type:"thinking",thinking:"hmm"},{type:"tool_use",name:"Bash"}]}}
        ,{type:"user",message:{content:[{type:"tool_result",content:"ok"}]}}
        ,{type:"assistant",message:{content:[{type:"text",text:"We decided to use SurrealKV because it needs no external process."}]}}' >"$td/t.jsonl"
cat >"$td/stub.sh" <<'STUB'
#!/usr/bin/env bash
cat >"$(dirname "$0")/prompt.txt"; n=$(( $(cat "$(dirname "$0")/calls" 2>/dev/null || echo 0) + 1 )); echo "$n" >"$(dirname "$0")/calls"
[ "$n" -gt 1 ] || { echo '{"memories": []}'; exit 0; }   # first call empty: hook must retry once
printf '```json\n{"memories":[{"content":"We decided to use SurrealKV because it needs no external process","tags":["decision"]},{"content":""}]}\n```\nNothing else worth keeping.\n'
STUB
chmod +x "$td/stub.sh"
export ALEXANDRIA_EXTRACT_CMD="$td/stub.sh" ALEXANDRIA_EXTRACT_MIN_CHARS=10 ALEXANDRIA_EXTRACT_FLUSH_WAIT=0 ALEXANDRIA_DETACHED=1   # run inline: assertions below are synchronous
stop() { jq -cn --arg s "$sess" --arg t "$td/t.jsonl" '{session_id:$s,transcript_path:$t,stop_hook_active:false}' | ./alexandria-extract.sh; }
stop
grep -q '^\[User\]: which storage engine' "$td/prompt.txt"
grep -q '^\[Assistant\]: We decided' "$td/prompt.txt"
! grep -q 'ignore me\|hmm\|tool_result' "$td/prompt.txt"
grep -q 'User correction: jj instead of git' "$td/prompt.txt"   # already-stored block
got=$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq -c '[.memories[] | select(.tags|index("extracted")) | {content,tags}]')
echo "$got"
[ "$got" = '[{"content":"We decided to use SurrealKV because it needs no external process","tags":["decision","extracted"]}]' ]
# No new transcript lines: LLM not called again. New short line: deferred (marker unchanged).
stop; [ "$(cat "$td/calls")" = 2 ]
jq -cn '{type:"user",message:{content:"ok"}}' >>"$td/t.jsonl"
ALEXANDRIA_EXTRACT_MIN_CHARS=1500 stop; [ "$(cat "$td/calls")" = 2 ]; [ "$(cat "$XDG_RUNTIME_DIR/alexandria/$sess.extracted")" = 5 ]
# stop_hook_active / child guard: no call.
jq -cn --arg s "$sess" --arg t "$td/t.jsonl" '{session_id:$s,transcript_path:$t,stop_hook_active:true}' | ./alexandria-extract.sh
[ "$(cat "$td/calls")" = 2 ]
# Headless session: no call, marker untouched.
jq -cn '{type:"user",message:{content:"headless chatter that must not be extracted"}}' >>"$td/t.jsonl"
CLAUDE_CODE_ENTRYPOINT=sdk-py stop; [ "$(cat "$td/calls")" = 2 ]; [ "$(cat "$XDG_RUNTIME_DIR/alexandria/$sess.extracted")" = 5 ]
# Empty twice: exactly two calls, nothing stored.
cat >"$td/empty.sh" <<'STUB'
#!/usr/bin/env bash
echo "$(( $(cat "$(dirname "$0")/calls2" 2>/dev/null || echo 0) + 1 ))" >"$(dirname "$0")/calls2"
echo '{"memories": []}'
STUB
chmod +x "$td/empty.sh"
jq -cn '{type:"user",message:{content:"purely tactical chatter, nothing durable here"}}' >>"$td/t.jsonl"
ALEXANDRIA_EXTRACT_CMD="$td/empty.sh" stop; [ "$(cat "$td/calls2")" = 2 ]
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("extracted"))] | length')" = 1 ]
# Detach: without ALEXANDRIA_DETACHED the hook returns at once; the work finishes in a detached copy.
cat >"$td/slow.sh" <<'STUB'
#!/usr/bin/env bash
sleep 2; echo '{"memories":[{"content":"Detached extraction outlives the hook","tags":[]}]}'
STUB
chmod +x "$td/slow.sh"
jq -cn '{type:"user",message:{content:"enough new text that the marker advances again"}}' >>"$td/t.jsonl"
start=$SECONDS
ALEXANDRIA_DETACHED='' ALEXANDRIA_EXTRACT_CMD="$td/slow.sh" stop
[ $((SECONDS - start)) -le 1 ]
found=
for _ in $(seq 20); do
  ./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq -e '.memories[] | select(.content == "Detached extraction outlives the hook")' >/dev/null && { found=1; break; }
  sleep 0.5
done
[ -n "$found" ]
echo OK
