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

## Server

- **`get_session` returns soft-deleted memories.** Found 2026-09-08: `SessionRepo::get_memories`
  (`crates/alexandria-storage/src/repos/session_repo.rs`) walks the `contains_session_memory` edges and
  fetches each fact with `get_fact`, which does not check `deleted`, and the response carries no `deleted`
  field. A memory deleted via `delete_memory` still shows up in `get_session` (verified: store, delete,
  `get_session` still lists it) while search correctly hides it. Knock-ons: the extract hook's
  "already stored" dedup list includes deleted memories; `session.memory_count` is never decremented;
  `test.sh` cleanup only works because the id is still listed. Done 2026-09-08: `get_memories` now
  drops deleted facts, which also fixes session-scoped `retrieve_memories` (same path, was leaking
  too); `get_session` reports `memory_count` as the live list length instead of the stored
  write-counter, which is left as is (`delete_memory` has no session to decrement).
- **`session.memory_count` column is now unread.** After the 2026-09-08 change above, nothing
  outside the storage tests reads it; `touch` still increments it on every store. Harmless. Drop the
  column and the `touch` increment in a future migration, or keep it as a "memories written" stat.
- **`get_memories` is N+1.** It selects the edge rows, then calls `get_fact` once per id. Fine at
  session sizes seen so far (tens of memories). Done 2026-09-08: replaced with one graph traversal,
  `SELECT * FROM $sess->contains_session_memory->fact WHERE deleted = false ORDER BY created_at`
  (same shape as `ClusterRepo::get_members`), after a `find_by_external_id` lookup. Two round trips
  regardless of session size; the Rust-side filter and sort are gone.
- **`get_memories` tiebreak on equal `created_at` is unspecified.** After the 2026-09-08 rewrite the
  ordering comes from `ORDER BY created_at`; the old Rust stable sort preserved edge order for facts
  created in the same instant. Only matters for bulk inserts within one tick (e.g. `import_document`
  chunks landing in the same session). Add `, id` as a secondary key if chunk order ever looks shuffled.

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
  (pre-retry) on 2026-09-08 because the README says `cp`. Done 2026-09-08: the README install snippet
  now symlinks the three scripts from the repo (`ln -sf`); `readlink -f` in the extract hook resolves
  the sibling path, so nothing else changed.
- **`test.sh` `empty.sh` stub emits unquoted JSON.** Found 2026-09-08: bash `printf` turns `\"` into a
  bare quote, so the stub prints `{memories: []}`; the "empty twice" check passes only because a parse
  failure and an empty result look the same to the hook. Done 2026-09-08: rewritten as a quoted
  heredoc like the other stubs; the check now passes on real JSON.
- **`"async": true` on the Stop hook is now redundant.** The hook returns in milliseconds since it
  detaches itself (2026-09-08), so the flag no longer buys anything. Done 2026-09-08: dropped from the
  README snippet, the README paragraph, and the local `~/.claude/settings.json`.
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
  runs on each of the five Stop calls in the harness. Done 2026-09-08: the wait is
  `ALEXANDRIA_EXTRACT_FLUSH_WAIT` (default 1) and `test.sh` sets it to 0.
- **Remaining `test.sh` time is the detach check.** After the flush-wait change (2026-09-08) the run is
  ~4 s: the `slow.sh` stub sleeps 2 s and the poll loop adds up to 0.5 s. Not worth touching; noted so
  nobody hunts for another `sleep 1`.
- **Hook development in a live interactive session pollutes the real database.** Companion to the
  headless item below: the installed Stop hook extracts from this session's transcript too, so stub
  payloads and probe strings from tests pasted into the conversation become `extracted` memories (a
  "Detach debug probe" memory from 2026-09-08 surfaced in auto-recall today). `test.sh` itself is
  clean (own session id, deletes on exit); the leak is the interactive session around it. Mitigation
  is the same: `ALEXANDRIA_AUTO_STORE=off` in the developing session's env, or delete by hand.
- **A queued follow-up prompt lands in the previous turn's chunk.** If the user types the next
  prompt while a turn is still generating, Claude Code dispatches it as soon as the turn ends, inside
  the 1 s flush wait, so the extract hook sees it with the previous turn. Harmless (it is extracted
  once, just one turn early); noted so it is not mistaken for a marker bug.
- **Per-prompt MCP handshake latency.** Measured 2026-09-08: recall hook end to end (initialize,
  initialized, tools/call with query embedding, DELETE) against the local service, 10 runs,
  median 88 ms, max 90 ms. Closed; nothing to optimise.
