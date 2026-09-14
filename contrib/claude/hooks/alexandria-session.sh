#!/usr/bin/env bash
# Claude Code PreToolUse hook (matcher: mcp__alexandria__store_memory).
# Injects the Claude Code session_id into store_memory calls that don't set one,
# so memories are grouped per session without relying on the model to pass it.
# Prints nothing when session_id is already set. Always exits 0 (never blocks).
set -uo pipefail
[ -z "${ALEXANDRIA_HOOK_CHILD:-}" ] || exit 0
jq -c 'select((.tool_input.session_id // "") == "" and (.session_id // "") != "")
  | {hookSpecificOutput:{hookEventName:"PreToolUse",updatedInput:(.tool_input + {session_id:.session_id})}}' 2>/dev/null
exit 0
