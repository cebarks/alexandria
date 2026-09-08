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
  Re-measure and lower, or fix alongside the model change above.

## Claude Code integration

- **No auto-recall equivalent for Claude Code.** The Pi extension hooks
  `before_agent_start`. The Claude Code analogue is a `UserPromptSubmit` hook that POSTs to
  `/mcp` and prints hits to stdout. Only worth doing if the server `instructions` + skill
  prove insufficient in practice.
- **`session_id` is never populated from Claude Code.** Nothing on the client side passes a
  session identifier to `store_memory`, so `get_session` / `finalize_session` are unused
  there. A hook could inject the Claude Code session id if per-session grouping matters.

## Minor

- `retrieve_memories` result JSON is returned as a stringified blob inside a `text` content
  block. Works, but structured `structuredContent` output would let clients render it.
- Debug UI cluster page shows cohesion `0` for single-member clusters; probably fine, but
  undocumented.
