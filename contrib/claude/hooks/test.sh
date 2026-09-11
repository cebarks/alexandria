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
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq -r '.session.agent_id')" = claude-code ]   # hook stores stamp the session
# Auto-store off: nothing new.
ALEXANDRIA_AUTO_STORE=off hook "never use tabs"
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("auto-detected"))] | length')" = 3 ]
# Headless session (CLAUDE_CODE_ENTRYPOINT=sdk-*): detectors off by default, on with ALEXANDRIA_AUTO_STORE=on.
CLAUDE_CODE_ENTRYPOINT=sdk-cli hook "never use spaces"
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("auto-detected"))] | length')" = 3 ]
CLAUDE_CODE_ENTRYPOINT=sdk-cli ALEXANDRIA_AUTO_STORE=on hook "never use spaces"
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("auto-detected"))] | length')" = 4 ]

# Session hook: injects session_id and agent_id when missing, silent when both present.
out=$(jq -cn '{session_id:"sess-test-123",tool_name:"mcp__alexandria__store_memory",tool_input:{content:"x"}}' | ./alexandria-session.sh)
[ "$(jq -r '.hookSpecificOutput.updatedInput.session_id' <<<"$out")" = "sess-test-123" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.agent_id' <<<"$out")" = "claude-code" ]
[ "$(jq -r '.hookSpecificOutput.updatedInput.content' <<<"$out")" = "x" ]
[ -z "$(jq -cn '{session_id:"s",tool_input:{content:"x",session_id:"already",agent_id:"other"}}' | ./alexandria-session.sh)" ]
# Set values are kept; only the missing one is filled.
[ "$(jq -cn '{session_id:"s",tool_input:{content:"x",session_id:"already"}}' | ./alexandria-session.sh | jq -c '.hookSpecificOutput.updatedInput')" = '{"content":"x","session_id":"already","agent_id":"claude-code"}' ]
[ "$(jq -cn '{session_id:"s",tool_input:{content:"x",agent_id:"other"}}' | ./alexandria-session.sh | jq -c '.hookSpecificOutput.updatedInput')" = '{"content":"x","agent_id":"other","session_id":"s"}' ]
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
        ,{type:"user",message:{content:[{type:"tool_result",is_error:true,content:"<tool_use_error>File has not been read yet.</tool_use_error>"}]}}
        ,{type:"user",message:{content:[{type:"tool_result",is_error:true,content:[{type:"text",text:"Exit code 101\nerror[E0433]: failed to resolve: use of undeclared crate"}]}]}}
        ,{type:"attachment",attachment:{type:"hook_additional_context",hookEvent:"UserPromptSubmit",content:["Relevant memories retrieved automatically from Alexandria for this prompt:\n- (similarity 0.61, id fact:abc) [hook-test] Recalled from another session: storage engines are compared on process count\n\nThese are surfaced proactively; verify relevance before relying on them, and use update_memory if any is stale."]}}
        ,{type:"attachment",attachment:{type:"hook_additional_context",hookEvent:"SessionStart",content:["PONYTAIL MODE ACTIVE"]}}
        ,{type:"assistant",message:{content:[{type:"text",text:"We decided to use SurrealKV because it needs no external process."}]}}' >"$td/t.jsonl"
cat >"$td/stub.sh" <<'STUB'
#!/usr/bin/env bash
cat >"$(dirname "$0")/prompt.txt"; echo "$(( $(cat "$(dirname "$0")/calls" 2>/dev/null || echo 0) + 1 ))" >"$(dirname "$0")/calls"
printf '```json\n{"memories":[{"content":"We decided to use SurrealKV because it needs no external process","tags":["decision"]},{"content":""}]}\n```\nNothing else worth keeping.\n'
STUB
chmod +x "$td/stub.sh"
export ALEXANDRIA_EXTRACT_CMD="$td/stub.sh" ALEXANDRIA_EXTRACT_MIN_CHARS=10 ALEXANDRIA_EXTRACT_FLUSH_WAIT=0 ALEXANDRIA_DETACHED=1   # run inline: assertions below are synchronous
stop() { jq -cn --arg s "$sess" --arg t "$td/t.jsonl" '{session_id:$s,transcript_path:$t,stop_hook_active:false}' | ./alexandria-extract.sh; }
stop
grep -q '^\[User\]: which storage engine' "$td/prompt.txt"
grep -q '^\[Assistant\]: We decided' "$td/prompt.txt"
grep -q 'ignore me\|hmm\|tool_result' "$td/prompt.txt" && exit 1   # `! cmd` never trips set -e
grep -q '^\[Tool error\]: Exit code 101' "$td/prompt.txt"   # is_error results are fed in; harness <tool_use_error> ones are not
grep -q 'tool_use_error\|has not been read' "$td/prompt.txt" && exit 1
grep -q 'User correction: jj instead of git' "$td/prompt.txt"   # already-stored block
# Auto-recall hits from the transcript's attachment lines join the already-stored block, never the conversation.
sed -n '/<already_stored>/,/<\/already_stored>/p' "$td/prompt.txt" | grep -q '^- (id fact:abc) \[hook-test\] Recalled from another session'
sed -n '/<conversation>/,/<\/conversation>/p' "$td/prompt.txt" | grep -q 'Recalled from another session\|PONYTAIL\|surfaced proactively' && exit 1
grep -q 'PONYTAIL' "$td/prompt.txt" && exit 1
got=$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq -c '[.memories[] | select(.tags|index("extracted")) | {content,tags}]')
echo "$got"
[ "$got" = '[{"content":"We decided to use SurrealKV because it needs no external process","tags":["decision","extracted"]}]' ]
# No new transcript lines: LLM not called again. New short line: deferred (marker unchanged).
stop; [ "$(cat "$td/calls")" = 1 ]
jq -cn '{type:"user",message:{content:"ok"}}' >>"$td/t.jsonl"
ALEXANDRIA_EXTRACT_MIN_CHARS=1500 stop; [ "$(cat "$td/calls")" = 1 ]; [ "$(cat "$XDG_RUNTIME_DIR/alexandria/$sess.extracted")" = 9 ]
# stop_hook_active / child guard: no call.
jq -cn --arg s "$sess" --arg t "$td/t.jsonl" '{session_id:$s,transcript_path:$t,stop_hook_active:true}' | ./alexandria-extract.sh
[ "$(cat "$td/calls")" = 1 ]
# Headless session: no call, marker untouched.
jq -cn '{type:"user",message:{content:"headless chatter that must not be extracted"}}' >>"$td/t.jsonl"
CLAUDE_CODE_ENTRYPOINT=sdk-py stop; [ "$(cat "$td/calls")" = 1 ]; [ "$(cat "$XDG_RUNTIME_DIR/alexandria/$sess.extracted")" = 9 ]
# Empty result: exactly one call, nothing stored.
cat >"$td/empty.sh" <<'STUB'
#!/usr/bin/env bash
echo "$(( $(cat "$(dirname "$0")/calls2" 2>/dev/null || echo 0) + 1 ))" >"$(dirname "$0")/calls2"
echo '{"memories": []}'
STUB
chmod +x "$td/empty.sh"
jq -cn '{type:"user",message:{content:"purely tactical chatter, nothing durable here"}}' >>"$td/t.jsonl"
ALEXANDRIA_EXTRACT_CMD="$td/empty.sh" stop; [ "$(cat "$td/calls2")" = 1 ]
[ "$(./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq '[.memories[] | select(.tags|index("extracted"))] | length')" = 1 ]
# Detach: without ALEXANDRIA_DETACHED the hook returns at once; the work finishes in a detached copy.
cat >"$td/slow.sh" <<'STUB'
#!/usr/bin/env bash
sleep 2; echo '{"memories":[{"content":"Detached extraction outlives the hook","tags":[]}]}'
STUB
chmod +x "$td/slow.sh"
jq -cn '{type:"user",message:{content:"enough new text that the marker advances again"}}' >>"$td/t.jsonl"
start=$SECONDS
HOME=$td ALEXANDRIA_DETACHED='' ALEXANDRIA_EXTRACT_CMD="$td/slow.sh" stop   # HOME: log lands in $td, not the real one
[ $((SECONDS - start)) -le 1 ]
found=
for _ in $(seq 20); do
  ./alexandria-recall.sh get_session "$(jq -cn --arg s "$sess" '{session_id:$s}')" | jq -e '.memories[] | select(.content == "Detached extraction outlives the hook")' >/dev/null && { found=1; break; }
  sleep 0.5
done
[ -n "$found" ]
# Log rotation: an oversize log is renamed to .1 by the parent before it re-execs. stop_hook_active
# makes the detached child exit at once, so only the synchronous parent path is under test.
[ -f "$td/.cargo/logs/alexandria/extract.log" ]
head -c 1048576 /dev/zero >"$td/.cargo/logs/alexandria/extract.log"
jq -cn --arg s "$sess" --arg t "$td/t.jsonl" '{session_id:$s,transcript_path:$t,stop_hook_active:true}' | HOME=$td ALEXANDRIA_DETACHED='' ./alexandria-extract.sh
[ "$(stat -c %s "$td/.cargo/logs/alexandria/extract.log.1")" = 1048576 ]
[ "$(stat -c %s "$td/.cargo/logs/alexandria/extract.log")" = 0 ]
echo OK
