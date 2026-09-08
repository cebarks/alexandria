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

- **Error-resolution tracker not ported.** `contrib/claude/hooks/` now has auto-recall,
  session_id injection, correction/preference detectors, and Stop-hook LLM extraction
  (plan: `docs/plans/2026-09-08-todo-misc-plan.md`). The Pi error-resolution detector was
  deliberately skipped: it needs PostToolUse state across a turn and yields low-signal
  "Error with X / Resolution: <200 chars>" memories; the extraction pass captures root causes
  once resolved. Revisit only if extracted memories turn out to miss resolved errors.
- **Hooks read env vars only, not `client.toml`.** Won't fix (2026-09-08): bash has no TOML parser
  and a `yq`/`tomlq` dependency for a handful of values is worse than env vars in `settings.json`.
  Documented in `contrib/claude/README.md`.
- **Extraction is one-shot per turn and non-deterministic.** The Stop hook writes its marker before
  calling haiku, so each transcript chunk gets exactly one extraction attempt. Measured 2026-09-08
  on the same 9.7k-char prompt: first run returned `{"memories": []}`, second run returned three good
  memories. Options if recall quality suffers: retry once on an empty result, raise the temperature
  floor by asking for "at least N candidates", or switch `ALEXANDRIA_EXTRACT_MODEL` to sonnet.
- **Extraction wall time is 15–45 s against a 90 s hook timeout.** Most of it is `claude -p`
  startup plus haiku latency, not prompt size (9.7k chars). If long sessions hit the timeout, raise
  `timeout` in `settings.json` or lower the 64,000-char cap in `alexandria-extract.sh`. Also
  untested: whether `claude -p` inside a Stop hook holds the user's turn visibly for that long, or
  whether it should be backgrounded (`nohup ... &` with the marker still written up front).
- **Manual in-UI checks still pending.** Tested only by piping hook JSON into the installed scripts.
  Not yet observed in a live Claude Code session: the `systemMessage` warning rendering when the
  server is down, `updatedInput` from `alexandria-session.sh` being honoured without a
  `permissionDecision`, and the Stop hook firing on a real turn.
- **Per-prompt MCP handshake latency.** Measured 2026-09-08: recall hook end to end (initialize,
  initialized, tools/call with query embedding, DELETE) against the local service, 10 runs,
  median 88 ms, max 90 ms. Closed; nothing to optimise.
