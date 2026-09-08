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
- **Extraction is non-deterministic.** Measured 2026-09-08 on the same 9.7k-char prompt: first run
  returned `{"memories": []}`, second run returned three good memories. Done 2026-09-08: the Stop hook
  now retries once on an empty or failed result within its 80 s budget. Remaining options if recall
  quality still suffers: ask for "at least N candidates", or switch `ALEXANDRIA_EXTRACT_MODEL` to sonnet.
- **Extraction wall time is 15–45 s per call, up to 80 s with the retry.** Most of it is `claude -p`
  startup plus haiku latency, not prompt size (9.7k chars). Done 2026-09-08: Stop hooks block the
  turn by default, so the hook now runs with `"async": true` (native Claude Code option, no hook
  timeout enforced, no `nohup` needed). If the script's own 80 s budget is too tight, lower the
  64,000-char cap in `alexandria-extract.sh`.
- **Async extraction is lost on session teardown.** Claude Code kills async hooks still running when
  the session ends (verified 2026-09-08 headless, two-turn `--input-format stream-json` session: the
  hook starts ~45 ms after the turn ends and a child still running at teardown never finishes).
  Done 2026-09-08: the hook re-execs itself with `setsid -f` right after reading stdin and returns;
  the detached copy does the wait, LLM call, and store (detaching only `claude -p` would not do, the
  parse-and-store step runs in the script after it). Guard env `ALEXANDRIA_DETACHED`; `test.sh` sets
  it to run inline and has one fork check. Re-verified headless: a 10 s stub finished 11 s after the
  session exited. Stderr of the detached copy goes to `$XDG_RUNTIME_DIR/alexandria/extract.log`.
- **Stop fires before the transcript has the final assistant message.** Found 2026-09-08: the hook
  read 215 lines while the turn's last assistant text was line 216 (flushed ~50 ms later), so every
  extraction ran one assistant message late and a session's last reply was never seen (the
  previous session's final turn came to 1293 chars without it, under the 1500 threshold, and was
  deferred into oblivion). Fixed 2026-09-08 with a `sleep 1` before reading the transcript (async, so
  free). Alternative if the wait ever proves too short: the Stop hook input carries
  `last_assistant_message`; append it to the chunk instead.
- **Retry doubles cost on tactical turns.** Every turn where haiku correctly finds nothing now pays a
  second call (up to ~80 s wall, hidden by async). Watch the `extracted` volume; if it is mostly
  noise or the cost matters, drop the retry or gate it on transcript size.
- **Installed hooks drift from the repo.** `~/.claude/hooks/alexandria-extract.sh` was found stale
  (pre-retry) on 2026-09-08 because the README says `cp`. Symlinking the three scripts from the repo
  instead would remove the step; update the README install snippet when next touched.
- **`test.sh` `empty.sh` stub emits unquoted JSON.** Found 2026-09-08: bash `printf` turns `\"` into a
  bare quote, so the stub prints `{memories: []}`; the "empty twice" check passes only because a parse
  failure and an empty result look the same to the hook. Rewrite it as a quoted heredoc like the
  other stubs when `test.sh` is next touched.
- **`"async": true` on the Stop hook is now redundant.** The hook returns in milliseconds since it
  detaches itself (2026-09-08), so the flag no longer buys anything. Harmless; drop it from the README
  snippet and `settings.json` next time the install docs change.
- **Manual in-UI checks.** Done 2026-09-08, none pending: `updatedInput` from
  `alexandria-session.sh` is honoured without a `permissionDecision` (a `store_memory` call with no
  `session_id` from a live session landed under that session); the Stop hook fires on real turns
  (seven `extracted` memories under a live session id); the async Stop hook does not hold the turn
  (`turn_duration` logged 23 s before the extracted memories were stored); the server-down warning
  surfaces as an informational system message, "UserPromptSubmit says: Alexandria memory
  unavailable: cannot reach <url>" (seen headless via `--output-format stream-json`).
- **Headless experiments write to the live server.** Any `claude -p` run on this machine fires the
  installed hooks, so a stub extractor's output (or haiku's) lands in the real database under a
  throwaway session id; 2026-09-08 testing left six `stub` memories that had to be deleted by hand.
  Prefix experiments with `ALEXANDRIA_AUTO_STORE=off` or point `ALEXANDRIA_URL` at a scratch server.
- **`test.sh` now takes ~6 s instead of ~1.5 s.** The `sleep 1` flush wait in `alexandria-extract.sh`
  runs on each of the five Stop calls in the harness. Fine for a manual check; if it ever matters,
  make the wait an env var and set it to 0 in the test.
- **A queued follow-up prompt lands in the previous turn's chunk.** If the user types the next
  prompt while a turn is still generating, Claude Code dispatches it as soon as the turn ends, inside
  the 1 s flush wait, so the extract hook sees it with the previous turn. Harmless (it is extracted
  once, just one turn early); noted so it is not mistaken for a marker bug.
- **Per-prompt MCP handshake latency.** Measured 2026-09-08: recall hook end to end (initialize,
  initialized, tools/call with query embedding, DELETE) against the local service, 10 runs,
  median 88 ms, max 90 ms. Closed; nothing to optimise.
