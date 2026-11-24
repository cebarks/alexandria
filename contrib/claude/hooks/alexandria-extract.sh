#!/usr/bin/env bash
# Deprecated Stop-hook entrypoint retained for safe migration of existing Claude settings.
# Automatic transcript extraction and store_memory writes are no longer supported.
# Remove this Stop hook from Claude settings and save durable memories with the explicit
# authenticated store_memory tool / Alexandria memory skill instead.
set -uo pipefail
input=$(cat) # consume stdin so Claude's hook writer cannot receive SIGPIPE
[ -z "${ALEXANDRIA_HOOK_CHILD:-}" ] || exit 0
if [ -n "${ALEXANDRIA_AUTO_STORE:-}" ] || [ -n "${ALEXANDRIA_EXTRACT_MODEL:-}" ] || [ -n "${ALEXANDRIA_EXTRACT_TOOL_ERRORS:-}" ]; then
  echo "Alexandria: automatic extraction settings are obsolete and ignored; remove the Stop hook and use explicit store_memory." >&2
fi
exit 0
