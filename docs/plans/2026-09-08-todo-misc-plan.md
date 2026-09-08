# TODO-misc implementation plan (2026-09-08)

Covers every open item in `TODO-misc.md` after the threshold and structured-output work.
Items are split into independently shippable tasks, ordered by dependency and value.
Everything in Track A is bash + jq in `contrib/claude/hooks/`, tested against the
running `alexandria` systemd user service. Track C is the only architectural piece and
gets its own spec before implementation.

Facts this plan relies on (verified 2026-09-08):

- Claude Code hook stdin carries `session_id`, `transcript_path`, `cwd` on every event;
  `prompt` on UserPromptSubmit; `tool_name`/`tool_input` on PreToolUse;
  `last_assistant_message` and `stop_hook_active` on Stop; `reason` on SessionEnd.
- PreToolUse may return `hookSpecificOutput.updatedInput` to rewrite an MCP tool call.
  Matcher for our tool is the literal `mcp__alexandria__store_memory`.
- Stop hooks have a 600 s budget; SessionEnd hooks share a 1.5 s budget. LLM work must
  run on Stop, not SessionEnd.
- Hook JSON output supports a top-level `systemMessage` (user-facing warning) and
  `hookSpecificOutput.additionalContext` (model-facing context).
- Transcript is JSONL. Lines have `type` of `user` or `assistant` and
  `message.content` that is either a string or an array of blocks with `type` in
  `text`, `tool_use`, `tool_result`, `thinking`. User lines can also be local-command
  caveats and hook injections; filter those out by prefix.
- `claude -p "<prompt>" --model haiku` runs from inside a Claude Code child process
  (tested). It will fire hooks itself, so every hook needs a recursion guard.
- `get_session` returns every memory stored with a given `session_id`; it is the
  "already stored this session" list for extraction dedup.
- The Pi extension (`contrib/pi/extensions/alexandria-auto-recall`) is the reference
  for detector regexes and the extraction prompt. Pi code is not modified.

---

## Track A: Claude Code hooks

### A0. Point Claude Code at the service (setup, no repo change)

Prerequisite for testing every other task in this track.

- `claude mcp add --transport http --scope user alexandria http://127.0.0.1:3000/mcp`
- Copy `contrib/claude/hooks/alexandria-recall.sh` to `~/.claude/hooks/`, add the
  UserPromptSubmit block from `contrib/claude/README.md` to `~/.claude/settings.json`.
- Copy `contrib/claude/skills/alexandria-memory` to `~/.claude/skills/`.
- Verify: new Claude Code session, ask a question, see "Relevant memories" injected, and
  `mcp__alexandria__store_memory` shows in `/mcp`.

### A1. Recall hook: JSON output, surface failures to the user

Bounded. File: `contrib/claude/hooks/alexandria-recall.sh`, `contrib/claude/hooks/test.sh`,
`contrib/claude/README.md`.

- Switch the hook's stdout from plain text to the hook JSON shape. Success path emits
  `{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"<memories block>"}}`.
- On any failure (unreachable, bad response, timeout) emit
  `{"systemMessage":"Alexandria memory unavailable: <one-line reason>"}` and exit 0.
  Prompt still proceeds. This replaces the stderr-only note.
- Add `[ -n "${ALEXANDRIA_HOOK_CHILD:-}" ] && exit 0` at the top (recursion guard,
  used by A4).
- `test.sh`: unreachable-server case now asserts the systemMessage JSON; success case
  asserts `additionalContext` contains the seeded fact.
- Manual check: `systemctl --user stop alexandria`, send a prompt, confirm the warning
  is visible in the Claude Code UI. If `systemMessage` turns out not to render for
  UserPromptSubmit, fall back to putting the warning in `additionalContext` and note
  that in the README.

### A2. PreToolUse hook: inject `session_id` into `store_memory`

Bounded. New file `contrib/claude/hooks/alexandria-session.sh`; edits to
`alexandria-recall.sh`, `test.sh`, `README.md`.

- Hook on `PreToolUse` with matcher `mcp__alexandria__store_memory`. Reads stdin; if
  `tool_input.session_id` is absent or empty, outputs
  `{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":<tool_input + session_id>}}`.
  If already set, outputs nothing. Never sets `permissionDecision`, so the normal
  permission flow is untouched.
- Remove the "Alexandria session_id for this conversation" line from the recall hook's
  output; it is now redundant and costs context on every prompt.
- Update `test.sh`: drop the session_id grep from the recall test; add a case that pipes
  a PreToolUse payload without session_id through the new hook and asserts the
  `updatedInput.session_id` field.
- Manual check: in Claude Code, ask it to store a memory without mentioning sessions,
  then `get_session` with the Claude Code session id and see the memory listed.

Open question to confirm during implementation: whether `updatedInput` alone is honoured
without `permissionDecision`. If not, the hook returns `permissionDecision: "ask"` plus
`updatedInput`, which preserves the prompt.

### A3. Heuristic detectors on UserPromptSubmit (correction, preference)

Bounded. Edits to `alexandria-recall.sh` (renamed responsibilities noted in README),
`test.sh`, `README.md`.

- Port `CORRECTION_PATTERNS` and `PREFERENCE_PATTERNS` from the Pi detectors to
  `grep -oiE` / `sed -E` in bash. Same length gates (8 to 500 chars), same prefixes
  (`User correction: ...`, `User preference: ...`), same tags plus `auto-detected`.
- Runs inside the recall hook after recall, sharing the one MCP session, so there is
  still one handshake per prompt. Gated by `ALEXANDRIA_AUTO_STORE != off`.
- Refactor `mcp_call` into `mcp_open` / `mcp_tool` / `mcp_close` so one session can
  carry several tool calls. Keep the debug CLI mode working.
- Dedup: per-session file `${XDG_RUNTIME_DIR:-/tmp}/alexandria/<session_id>.stored`
  holding normalized (lowercased, whitespace-collapsed) contents. Skip if present.
- Stores carry `session_id` directly, so A2 is not needed for these.
- Error-resolution tracker: deliberately not ported. It needs PostToolUse state across a
  turn and produces low-signal "Error with X / Resolution: <200 chars>" memories. The
  extraction pass in A4 covers "root cause once resolved" better. Note this in
  `TODO-misc.md`.
- `test.sh`: seed nothing; pipe prompts "no, use jj instead of git" and "always run
  clippy before pushing" through the hook with a fake session id; `get_session` must
  return the two auto-detected memories; sending the same prompt again must not
  create a duplicate.

### A4. LLM extraction on Stop

Bounded but the largest hook. New file `contrib/claude/hooks/alexandria-extract.sh`,
`README.md`, `test.sh`.

- Hook on `Stop`, per-hook `timeout` 90 s. Exits immediately when
  `stop_hook_active` is true or `ALEXANDRIA_HOOK_CHILD` is set, or
  `ALEXANDRIA_AUTO_STORE=off`.
- Incremental: a marker file `${XDG_RUNTIME_DIR:-/tmp}/alexandria/<session_id>.extracted`
  holds the transcript line count at the last run. Only lines after it are considered.
  Skip if the new user+assistant text is under 1,500 characters, so short turns cost
  nothing and one haiku call covers several turns.
- Serialize with jq: user lines (string content or text blocks, skipping lines that are
  only `tool_result` and lines starting with `<local-command`, `<command-`,
  `<system-reminder`, or the recall hook's header) and assistant `text` blocks. Format
  as `[User]: ...` / `[Assistant]: ...`. Cap at the last 64,000 characters.
- Already-stored block: `get_session` for the current session id, contents only.
- Call `ALEXANDRIA_HOOK_CHILD=1 claude -p --model haiku --output-format text` with
  the Pi extraction prompt verbatim plus the two blocks. Strip code fences, parse
  `{"memories":[...]}` with jq, drop entries with empty content.
- Store each via `store_memory` with `session_id` and tags plus `extracted`.
- Fail open everywhere; on failure write one line to stderr and update the marker
  anyway so a broken turn is not retried forever.
- `test.sh`: build a small fake transcript JSONL in a temp dir with one clear durable
  fact ("we decided to use SurrealKV because it needs no external process"), run the
  hook with a fake session id and `ALEXANDRIA_EXTRACT_CMD` overridden to a stub that
  echoes a fixed JSON (so the test is deterministic and free), assert `get_session`
  shows the memory with the `extracted` tag. One manual run with the real `claude -p`
  documented in the README.
- Config surface (env only, per A5): `ALEXANDRIA_AUTO_STORE`,
  `ALEXANDRIA_EXTRACT_MODEL` (default `haiku`), `ALEXANDRIA_EXTRACT_MIN_CHARS`
  (default 1500).

### A5. Close the `client.toml` item as won't-fix

No code. Bash has no TOML parser and adding `yq`/`tomlq` as a dependency for three
values is worse than three env vars. Document in `contrib/claude/README.md` that the
hooks are env-configured and that `settings.json` `env` is the place to set them.
Remove the item from `TODO-misc.md`.

### A6. Measure per-prompt handshake latency, then close or promote

No code unless the number is bad. `time` the recall hook ten times against the live
service with a realistic prompt, record median in `TODO-misc.md`. Under 300 ms: close
the item. Over that: the fix is to keep one MCP session id in
`${XDG_RUNTIME_DIR}/alexandria/mcp-session` and reuse it until a 404, which is a
separate bounded task.

---

## Track B: server and docs

### B1. Verify the "cohesion 0 for single-member clusters" item

The cluster detail page already renders `N/A (fewer than 4 members)` for anything under
four members, and `check_cohesion` returns Healthy in that case. Reproduce against the
live debug UI with a one-member cluster. Expected outcome: the item is stale, remove it.
If a `0` does appear somewhere (dashboard or maintenance log), fix the one call site to
render `N/A` and add a unit test in `crates/alexandria-mcp/src/debug/`.

---

## Track C: asymmetric embedding model (architectural, separate spec)

Do not start until Track A is in use, so a real corpus exists to measure against.

### C1. Spike: measure candidate models

Throwaway example in `alexandria-pipeline` like this morning's, extended to
`BAAI/bge-small-en-v1.5` and `intfloat/e5-small-v2`. Both are BERT-family and 384-dim,
so `CandleProvider` should load them; e5 requires `query: ` / `passage: ` prefixes,
which means the provider needs an asymmetric embed API to test it at all. Output: the
same match/noise table for all three models on the synthetic pairs plus whatever real
memories exist by then. Deliverable is a recommendation, not code.

### C2. Spec: re-embed migration

Written only if C1 shows a clear winner. Must cover:

- An asymmetric embedding API (`embed_query` / `embed_passage`) on
  `EmbeddingProvider`, with MiniLM implementing both as the same call.
- A `alexandria reembed --model <id>` subcommand: loads the new model, re-embeds every
  non-deleted `fact` in batches, recomputes cluster centroids, updates `system_config`,
  all inside a single transaction or with a resumable marker.
- Bumping the locked-model check to allow the switch only via that subcommand.
- Re-measuring and re-setting the client thresholds, since 0.35 is a MiniLM number.

### C3. Change the default model and docs

After C2 ships and the local instance has been migrated. Update `config.rs` default,
`README.md`, `docs/configuration.md`, and the Pi annotation from this morning.

---

## Order

1. A0, A1, A2 (one short session; A1 and A2 are independent of each other)
2. A3, then A4 (A4 reuses the MCP session helpers from A3)
3. A5, A6, B1 (housekeeping, an hour total)
4. C1 after a week or two of real use, then C2/C3 if justified

## Not in scope

- Pi extension changes (annotated only, per earlier decision).
- Porting the error-resolution tracker (see A3).
- Adding any dependency beyond bash, curl, jq, and the `claude` CLI.
