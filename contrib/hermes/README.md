# Hermes companion

Optional client-side companion for using Alexandria from
[Hermes Agent](https://hermes-agent.nousresearch.com). Like the pi companion, it is not part of the
MCP server and is not required to use Alexandria. Hermes has a first-class external
*memory provider* slot, so recall and capture do not depend only on the agent choosing tools.
Selecting a provider is additive by default; the configuration below makes Alexandria the sole
active curated-memory backend without modifying Hermes.

What you get when it's active:

| Direction | Behaviour |
| --- | --- |
| Read | `prefetch()` retrieves global, cross-session memories for nontrivial prompts and applies the configured client similarity floor. Semantic results are cached briefly. |
| Write | Regex detectors ported from the pi companion (corrections, preferences, error→resolution pairs, tool-store tracking) capture during the session; an LLM extraction pass at session end stores durable facts via the host's own model. |
| Time | Primary foreground prefetch checks due reminders freshly, including semantic-cache hits. Hermes currently skips this hook on trivial prompts such as “thanks”; delivery waits for a nontrivial foreground prompt or an explicit check. Background/cron/flush/subagent paths never consume automatically. The four reminder operations are also native tools. |
| Tools | `alexandria_store_memory`, `alexandria_search_memories`, `alexandria_recall`, `alexandria_update_memory`, `alexandria_delete_memory`, plus the four reminder tools. |

## Requirements

- A running Alexandria HTTP server (see the root [README](../../README.md)); default
  `http://127.0.0.1:3000/mcp`.
- Hermes Agent with the memory-provider plugin surface (`agent/memory_provider.py`).

## Install

Copy (or symlink) the provider directory into Hermes' user plugin directory:

```sh
cp -r contrib/hermes/alexandria ~/.hermes/plugins/alexandria
```

Then activate it — either through the setup wizard (`hermes memory setup`, which walks the
provider's `get_config_schema()`), or directly:

```sh
hermes config set memory.provider alexandria
```

Only one external provider runs at a time. For the recommended Alexandria-only setup, disable
Hermes' built-in curated stores separately in `config.yaml`:

```yaml
memory:
  provider: alexandria
  memory_enabled: false
  user_profile_enabled: false
  alexandria:
    url: http://127.0.0.1:3000/mcp
    auto_store: true
    extract: true
    store_session: true
```

Keep the **`memory` toolset enabled**: disabling the whole toolset also hides external-provider
tools. The two false flags remove native MEMORY.md/USER.md injection and the built-in editing
tool, but preserve Alexandria's hooks/tools. Do not use `skip_memory`, which skips external
initialization too. The flags require Hermes with the supported built-in-memory-disabled surface.

Existing files, previously stored mirror facts, and SQLite conversation history are not migrated
or deleted. Hermes' `session_search` remains separate. To coexist with built-in memory instead,
leave its two store flags enabled; there is deliberately **no copying or synchronization** between
that store and Alexandria.

## Configuration

All keys live under `memory.alexandria` in Hermes' `config.yaml`:

| Key | Default | Meaning |
| --- | --- | --- |
| `url` | `http://127.0.0.1:3000/mcp` | Alexandria StreamableHTTP endpoint |
| `timeout` | `10.0` | Per-request HTTP timeout (seconds) |
| `recall_limit` | `5` | Max memories injected per turn |
| `min_similarity` | `0.45` | Client-side noise floor on top of the server's |
| `auto_store` | `true` | Automatically capture corrections, preferences and error-resolution pairs; explicit native tool writes are unaffected |
| `extract` | `true` | Run the LLM extraction pass at session end |
| `store_session` | `true` | Pass `session_id` on stores so Alexandria groups memories per conversation |
| `cache_ttl` | `60.0` | Recall cache lifetime (seconds); a failed recall falls back to the last cached text |

## How it maps to Hermes' hooks

| Hermes hook | Alexandria call |
| --- | --- |
| `prefetch(query)` | `retrieve_memories` (or `recall`); returned text is injected into context |
| `sync_turn(user, assistant, messages)` | detector pass → `store_memory`; tool-event scan feeds the error tracker and dedup buffer |
| `on_session_end(messages)` | flush error→resolution pairs, LLM extraction pass → `store_memory` + `finalize_session` |
| `get_tool_schemas()` / `handle_tool_call()` | native store/search/recall/update/delete and all four reminder tools |
| `on_session_switch(reset=True)` | ordered reset of per-session state; rewind preserves confirmed-store dedup |

Capture design follows the pi companion (see
[`contrib/pi/extensions/alexandria/README.md`](../pi/extensions/alexandria/README.md)): heuristics
during the session, one LLM pass at session end, one shared dedup buffer, fail-open everywhere —
a down or slow Alexandria costs recall, never the conversation. Background work runs on Hermes'
`spawn_context_thread` so profile multiplexing keeps working. Turn capture, explicit native tools,
and session-end work are ordered on that worker; session IDs and transcript snapshots are captured
before queueing. Tool-result identities, rather than transcript length, survive compression/rewind.
Caller waits are bounded: a timeout may leave accepted work running, so the provider retains its
state/client until that work completes and does not blindly retry writes. Failed stores do not
become extraction exclusions. Pending failed error-resolution writes can be attempted again by a
later in-process session-end call, but there is no durable retry spool across process exit.

## Tests

From the repository root, with Python 3.12+ and `httpx` installed:

```sh
python3 -m pip install httpx
just hermes-test
# Or, without installing into your current environment:
uv run --python 3.12 --with httpx just hermes-test
```

`contrib/hermes/run_tests.sh` also works without Hermes installed; set `PYTHON` to select
an interpreter. CI runs the same unittest discovery command as `just hermes-test`.

Plain `unittest`, no pytest. The suite runs against `mock_server.py`, an in-memory MCP
StreamableHTTP server that speaks the same wire protocol as Alexandria (initialize handshake,
session header, JSON and SSE response bodies) with naive word-overlap scoring standing in for
embeddings. The mock is a protocol double, not a reference implementation of Alexandria's ranking.
The provider tests import Hermes' real `MemoryProvider` ABC when a Hermes source tree is present
and fall back to `tests/_hermes_stub/` otherwise (CI).

## Native tool parameters

All native names start with `alexandria_`. `search_memories(query, limit)` maps to the
server's `retrieve_memories`; the client applies `min_similarity` locally. `recall` accepts
`query` and an optional returned `scope_handle` for narrowing. Update/delete/cancel use the
server's record IDs. Reminder scheduling uses `message` (not `content`) and exactly one of
`due_at`, `pattern`, or `cron`, with optional `target_project`, `prov_project`, and `note`.
Listing defaults to `pending`, with `delivered`, `cancelled`, and `all` also supported.
`check_reminders(project)` consumes deliveries; tell the user their returned messages.

## Known gaps

- Hermes currently skips provider prefetch on trivial prompts. The provider itself supports a
  direct trivial-query call, but that is not end-to-end host delivery coverage. Reminder notes,
  escalation, and missed-occurrence context are retained when delivery does run; a saturated
  count is rendered with `+`, not as an exact count. Non-primary contexts never claim automatically;
  explicit `alexandria_check_reminders` remains available.
- Reminder delivery is best-effort: the server claims a fire before returning it. Hermes can
  discard a foreground provider result on timeout while its thread continues, so a successful
  claim does not guarantee presentation. Consumed deliveries are never replayed from the
  semantic cache; the provider does not claim exactly-once user notification.

- Not exercised against a live Alexandria in CI — the mock pins the wire contract, not ranking
  behaviour. A smoke test against a real server is worth adding if this gains users.
- The extraction pass uses whatever model the host has active; quality varies with it, and it is
  skipped entirely if the host exposes no plugin LLM.
- Heat/Ebbinghaus ranking is inert server-side (see
  [`docs/performance-and-ability-findings.md`](../../docs/performance-and-ability-findings.md) A2),
  so recall orders by cosine similarity only.
