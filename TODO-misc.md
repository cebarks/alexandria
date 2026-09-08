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
- **Pi auto-recall default `min_similarity = 0.58`** (`contrib/pi`) is too high. Measured
  2026-09-08 on MiniLM with synthetic pairs: question-vs-matching-statement scores 0.40–0.65,
  unrelated memories 0.07–0.40. The Claude Code hook now defaults to the measured `0.35`;
  the Pi code and docs are annotated with that recommendation but deliberately left at `0.58`.
  Change `contrib/pi/extensions/alexandria-auto-recall/src/config.ts` and the three Pi doc
  tables when Pi is next touched.

## Claude Code integration

- **No auto-store for Claude Code.** `contrib/claude/hooks/` covers auto-recall and
  session_id injection (plan: `docs/plans/2026-09-08-todo-misc-plan.md`, A3/A4), but the Pi extension's heuristic detectors
  (correction/preference/error-resolution) and session-end LLM extraction have no Claude Code
  equivalent. Candidates: a `Stop`/`SessionEnd` hook for extraction, `UserPromptSubmit` for the
  detectors.
- **Recall hook reads env vars only, not `client.toml`.** Bash has no TOML parser; the Pi
  extension honours `$XDG_CONFIG_HOME/alexandria/client.toml`. Revisit if the hook grows enough
  config to matter.
- **Recall hook does a full MCP handshake per prompt.** Four localhost round trips
  (initialize, initialized, tools/call, DELETE) plus one query embedding, every prompt. Fine at
  human typing speed; revisit only if latency becomes noticeable.

## Minor

- Debug UI cluster page shows cohesion `0` for single-member clusters; probably fine, but
  undocumented.
