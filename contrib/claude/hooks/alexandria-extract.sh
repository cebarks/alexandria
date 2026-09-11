#!/usr/bin/env bash
# Claude Code Stop hook: LLM extraction of durable facts into Alexandria.
#
# Reads the hook JSON on stdin, serializes the transcript lines added since the
# last run, asks `claude -p` (haiku by default) for standalone memories using the
# same prompt as the Pi extension, and stores them with session_id and the
# `extracted` tag. The already-stored block holds this session's memories plus
# the auto-recall hits the transcript carries for each prompt (cross-session
# dedup). Incremental: a marker file holds the transcript line count at the last
# run; short turns accumulate until enough new text exists. Fails open and
# always exits 0.
#
# Env (all optional):
#   ALEXANDRIA_AUTO_STORE          "off" disables; "on" enables in headless (sdk-*) sessions, where the default is off
#   ALEXANDRIA_EXTRACT_MODEL       default haiku
#   ALEXANDRIA_EXTRACT_MIN_CHARS   default 1500; new text below this is deferred to a later turn
#   ALEXANDRIA_EXTRACT_FLUSH_WAIT  default 1; seconds to wait for the transcript to flush before reading it (tests set 0)
#   ALEXANDRIA_EXTRACT_CMD         override the LLM command (reads prompt on stdin, prints JSON); tests use a stub
#   ALEXANDRIA_MARKER_MAX_AGE_DAYS default 7; per-session markers idle longer than this are pruned
#   ALEXANDRIA_HOOK_CHILD          set by this hook on the `claude -p` child; every hook exits at once
#   ALEXANDRIA_DETACHED            set by this hook on its detached copy; tests set it to run inline
set -uo pipefail
input=$(cat)   # before any guard: exiting with stdin unread can SIGPIPE the writer (test.sh pipes jq in)
[ -z "${ALEXANDRIA_HOOK_CHILD:-}" ] || exit 0
# Headless sessions (`claude -p`, Agent SDK) inherit CLAUDE_CODE_ENTRYPOINT=sdk-*: auto-store is off
# there unless ALEXANDRIA_AUTO_STORE=on, so scripted experiments never land in the real database.
store=${ALEXANDRIA_AUTO_STORE:-}; [ -n "$store" ] || case "${CLAUDE_CODE_ENTRYPOINT:-}" in sdk-*) store=off;; esac
[ "$store" != off ] || exit 0

MCP="$(dirname "$(readlink -f "$0")")/alexandria-recall.sh"   # debug CLI mode = one-shot tool calls
MIN_CHARS="${ALEXANDRIA_EXTRACT_MIN_CHARS:-1500}"
CMD="${ALEXANDRIA_EXTRACT_CMD:-claude -p --model ${ALEXANDRIA_EXTRACT_MODEL:-haiku} --output-format text}"

# Claude Code kills hooks still running at session teardown, which would drop the last turn's
# extraction (15-80 s of LLM call). Re-exec detached: own session and process group, no inherited
# pipes, so neither a group kill nor pipe closure reaches it. The caller returns at once.
# Log and per-session markers live in the XDG state dir, so both outlive the login session; the log is
# rotated by size (one previous generation) and markers idle for over ALEXANDRIA_MARKER_MAX_AGE_DAYS
# (default 7) days are pruned before each re-exec. A detached copy already writing keeps its handle on the renamed file, so nothing interleaves.
state="${XDG_STATE_HOME:-$HOME/.local/state}/alexandria"
log="$state/extract.log"
[ -n "${ALEXANDRIA_DETACHED:-}" ] || {
  mkdir -p "$state"
  [ "$(stat -c %s "$log" 2>/dev/null || echo 0)" -lt 1048576 ] || mv -f "$log" "$log.1"
  find "$state" -maxdepth 1 \( -name '*.extracted' -o -name '*.stored' \) -mtime "+${ALEXANDRIA_MARKER_MAX_AGE_DAYS:-7}" -delete
  ALEXANDRIA_DETACHED=1 setsid -f "$0" <<<"$input" >/dev/null 2>>"$log"; exit 0; }
[ "$(jq -r '.stop_hook_active // false' <<<"$input")" = false ] || exit 0
session=$(jq -r '.session_id // ""' <<<"$input")
transcript=$(jq -r '.transcript_path // ""' <<<"$input")
[ -n "$session" ] && [ -r "$transcript" ] || exit 0

# Stop fires before the final assistant message is appended to the transcript (measured ~50 ms
# behind); without this wait every extraction runs one assistant message late and a session's
# last reply is never seen. Cheap: this copy is detached.
sleep "${ALEXANDRIA_EXTRACT_FLUSH_WAIT:-1}"
marker="$state/$session.extracted"
done_lines=$(cat "$marker" 2>/dev/null || echo 0)
total=$(wc -l <"$transcript")
[ "$total" -gt "$done_lines" ] || exit 0

# user (string or text blocks; skip injected/system lines) + assistant text + failed tool results.
# Errors go in so a silent fix-and-retry still shows the LLM the root cause; <tool_use_error> is the
# harness refusing a call (file not read, old_string missing), never durable knowledge. Each error is
# attributed to its tool_use (name + input, cut short) via tool_use_id: the call and its result land
# in the same turn, so the chunk is slurped and the id map built from its assistant lines.
text=$(tail -n +"$((done_lines + 1))" "$transcript" | jq -nrR '
  def txt: if type == "string" then . else [.[]? | select(.type == "text") | .text] | join("\n") end;
  [inputs | fromjson?] as $lines
  | ([$lines[] | select(.type == "assistant") | .message.content[]? | select(.type == "tool_use")
      | {key: .id, value: (.name + " " + (.input | .command // .file_path // tostring | .[:120]) + " -- ")}] | from_entries) as $tools
  | $lines[] | select(.type == "user" or .type == "assistant") | .type as $role | (.message.content // "")
  | ((txt | select(length > 0)
      | select(startswith("<local-command") or startswith("<command-") or startswith("<system-reminder") | not)
      | (if $role == "user" then "[User]: " else "[Assistant]: " end) + .),
     (.[]? | select(.type == "tool_result" and .is_error == true) | ($tools[.tool_use_id] // "") as $tool
      | .content | txt | select(startswith("<tool_use_error>") | not) | "[Tool error]: " + $tool + .[:300]))
  | . + "\n"')
[ ${#text} -ge "$MIN_CHARS" ] || exit 0
[ ${#text} -le 64000 ] || text=${text: -64000}

# Marker first: a broken or slow turn is never retried.
mkdir -p "${marker%/*}"; echo "$total" >"$marker"

# Cross-session dedup: the recall hook's hits for each prompt in this chunk sit in the transcript as
# hook_additional_context attachments, already filtered to the tuned threshold, so they go into the
# already-stored block at no cost. A post-hoc similarity filter on the candidates cannot replace this:
# on all-MiniLM-L6-v2 (measured 2026-09-08) real duplicates score 0.64-0.76 against each other and
# distinct neighbours 0.63-0.76. ponytail: covers only what the user's prompts recalled; a gotcha that
# surfaces purely from tool output, or auto-recall off, still dedups within the session only.
recalled=$(tail -n +"$((done_lines + 1))" "$transcript" | jq -rR '
  fromjson? | select(.type == "attachment") | .attachment
  | select(.type == "hook_additional_context" and .hookEvent == "UserPromptSubmit")
  | .content[]? | strings | select(startswith("Relevant memories retrieved automatically"))
  | split("\n")[] | select(startswith("- (similarity ")) | sub("^- \\(similarity [^,]*, "; "- (")' | sort -u)
stored=$("$MCP" get_session "$(jq -cn --arg s "$session" '{session_id:$s}')" 2>/dev/null \
  | jq -r '.memories[]?.content | "- " + .')
stored=$(printf '%s\n%s' "$stored" "$recalled" | sed '/^$/d')
[ -n "$stored" ] || stored="(nothing stored yet)"

# Prompt text is verbatim from contrib/pi/extensions/alexandria-auto-recall/src/extraction.ts.
prompt="You are a memory extraction system. Given a conversation between a user and an AI coding assistant, extract durable facts worth remembering across sessions.

Extract:
- User preferences and conventions (tooling choices, style rules, workflow habits)
- Architectural/design decisions AND their rationale
- Bug root causes once resolved (not symptoms)
- Non-obvious gotchas, footguns, or platform/library quirks
- Corrections the user gave about something the assistant got wrong

Do NOT extract:
- Ephemeral task details (file paths being edited, current branch name, etc.)
- Things already in the \"already stored\" list below
- Common knowledge or well-documented behavior
- Incomplete work or open questions

Each extracted memory must be a standalone statement that makes sense without this conversation. No \"as discussed above\", no pronouns without antecedents.

Respond with JSON only:
{
  \"memories\": [
    {\"content\": \"standalone statement\", \"tags\": [\"relevant\", \"tags\"]},
    ...
  ]
}

If nothing is worth extracting, respond with: {\"memories\": []}

Already stored, this session or recalled for its prompts (do not duplicate):
<already_stored>
$stored
</already_stored>

Conversation:
<conversation>
$text
</conversation>"

# shellcheck disable=SC2086  # CMD is deliberately word-split
out=$(ALEXANDRIA_HOOK_CHILD=1 timeout 80 $CMD <<<"$prompt" 2>/dev/null) || { echo "alexandria-extract: LLM call failed" >&2; exit 0; }
# Models often wrap the JSON in a ``` fence and add prose after it: keep the first fenced block.
# shellcheck disable=SC2016  # the backticks are a regex, not a command substitution
json=$(sed -n '/^```/,/^```/{/^```/d;p}' <<<"$out"); [ -n "$json" ] || json=$out
mems=$(jq -c '.memories[]? | select((.content|type) == "string" and .content != "")
  | {content, tags: ([.tags[]? | strings] + ["extracted"] | unique)}' <<<"$json" 2>/dev/null)
[ -n "$mems" ] || exit 0
while read -r m; do
  res=$("$MCP" store_memory "$(jq -c --arg s "$session" '. + {session_id:$s,agent_id:"claude-code"}' <<<"$m")") \
    || echo "alexandria-extract: store failed: $res" >&2
done <<<"$mems"
exit 0
