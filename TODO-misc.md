# TODO (misc)

Open items noticed while getting Alexandria running under Claude Code (2026-09-08).

## Retrieval quality

- **Embedding model is the real ceiling.** `all-MiniLM-L6-v2` is a symmetric similarity
  model. A natural-language question against a stored statement scores only ~0.1–0.2
  cosine ("which database does the project use" vs "the project uses SurrealDB" = 0.19),
  while a keyword hit scores ~0.6. The floor was lowered to 0.10 to compensate, but an
  asymmetric retrieval model (e.g. an msmarco/bge/e5 family model) would separate real
  matches from noise far better. Blocked on: model is locked on first boot, so switching
  the default needs a migration/re-embed story.
- **Auto-recall client default `min_similarity = 0.58`** (`contrib/pi`, `docs/configuration.md`)
  was chosen under the same "genuine matches score 0.6+" assumption that turned out wrong.
  With MiniLM that threshold will almost never inject anything for question-style prompts.
  Re-measure and lower, or fix alongside the model change above. The Claude Code hook
  (`contrib/claude/hooks/alexandria-recall.sh`) defaults to `0.15`, which is an unmeasured guess
  in the other direction — settle both on the same measured number.

## Claude Code integration

- **No auto-store for Claude Code.** `contrib/claude/hooks/alexandria-recall.sh` covers
  auto-recall and session_id injection, but the Pi extension's heuristic detectors
  (correction/preference/error-resolution) and session-end LLM extraction have no Claude Code
  equivalent. Candidates: a `Stop`/`SessionEnd` hook for extraction, `UserPromptSubmit` for the
  detectors.
- **Recall hook reads env vars only, not `client.toml`.** Bash has no TOML parser; the Pi
  extension honours `$XDG_CONFIG_HOME/alexandria/client.toml`. Revisit if the hook grows enough
  config to matter.
- **Recall hook `session_id` is advisory.** The hook can only print "pass this session_id to
  store_memory"; whether the agent actually does so is up to the model. A `PreToolUse` hook on
  `mcp__alexandria__store_memory` could inject it into the call itself.
- **Recall hook failures are silent to the user.** Errors go to stderr only; Claude Code shows
  nothing unless hook output is inspected. Pi surfaces a warning notification. No obvious
  Claude Code equivalent short of injecting a "memory unavailable" line into context.
- **Recall hook does a full MCP handshake per prompt.** Four localhost round trips
  (initialize, initialized, tools/call, DELETE) plus one query embedding, every prompt. Fine at
  human typing speed; revisit only if latency becomes noticeable.

## Minor

- `retrieve_memories` result JSON is returned as a stringified blob inside a `text` content
  block. Works, but structured `structuredContent` output would let clients render it.
- Debug UI cluster page shows cohesion `0` for single-member clusters; probably fine, but
  undocumented.
