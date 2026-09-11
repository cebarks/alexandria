#!/usr/bin/env bash
# Claude Code PreToolUse hook (matcher: mcp__alexandria__store_memory).
# Injects the Claude Code session_id and agent_id "claude-code" into store_memory
# calls that don't set them, so memories are grouped per session and attributed
# without relying on the model to pass either. Prints nothing when both are
# already set. Always exits 0 (never blocks).
set -uo pipefail
input=$(cat)   # before any guard: exiting with stdin unread can SIGPIPE the writer
[ -z "${ALEXANDRIA_HOOK_CHILD:-}" ] || exit 0
jq -c '.tool_input as $t | .session_id as $s
  | $t + {session_id: (if ($t.session_id // "") == "" then $s else $t.session_id end),
          agent_id:   (if ($t.agent_id   // "") == "" then "claude-code" else $t.agent_id end)}
  | with_entries(select(.value != null)) | select(. != $t)
  | {hookSpecificOutput:{hookEventName:"PreToolUse",updatedInput:.}}' <<<"$input" 2>/dev/null
exit 0
