# Hermes companion

Optional client-side companion for using Alexandria from
[Hermes Agent](https://hermes-agent.nousresearch.com). Like the pi companion, it is not part of the
MCP server and is not required to use Alexandria — but it goes further than pi's extension does:
Hermes has a first-class *memory provider* slot (the same interface Honcho/Mem0/etc. occupy), so
Alexandria becomes the agent's memory rather than a tool the agent may or may not reach for.

What you get when it's active:

| Direction | Behaviour |
| --- | --- |
| Read | `prefetch()` calls `retrieve_memories` before every model call (skipping trivial prompts) and injects the top matches into context. Results are cached briefly so retries don't re-query. |
| Write | Regex detectors ported from the pi companion (corrections, preferences, error→resolution pairs, tool-store tracking) capture during the session; an LLM extraction pass at session end stores durable facts via the host's own model. |
| Time | `set_reminder` / `check_reminders` / `list_reminders` / `cancel_reminder` are exposed as native Hermes tools. |
| Tools | `alexandria_store_memory`, `alexandria_search_memories`, `alexandria_recall`, `alexandria_update_memory`, `alexandria_delete_memory`, plus the four reminder tools. |
| Mirror | Built-in MEMORY.md/USER.md writes are mirrored into Alexandria (tagged `source:builtin-memory`) so the two stores don't silently diverge. |

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

The built-in MEMORY.md/USER.md store stays active alongside it; only one external provider runs at
a time.

## Configuration

All keys live under `memory.alexandria` in Hermes' `config.yaml`:

| Key | Default | Meaning |
| --- | --- | --- |
| `url` | `http://127.0.0.1:3000/mcp` | Alexandria StreamableHTTP endpoint |
| `timeout` | `10.0` | Per-request HTTP timeout (seconds) |
| `recall_limit` | `5` | Max memories injected per turn |
| `min_similarity` | `0.45` | Client-side noise floor on top of the server's |
| `auto_store` | `true` | Run the regex detectors during sessions |
| `extract` | `true` | Run the LLM extraction pass at session end |
| `store_session` | `true` | Pass `session_id` on stores so Alexandria groups memories per conversation |
| `cache_ttl` | `60.0` | Recall cache lifetime (seconds); a failed recall falls back to the last cached text |

## How it maps to Hermes' hooks

| Hermes hook | Alexandria call |
| --- | --- |
| `prefetch(query)` | `retrieve_memories` (or `recall`); returned text is injected into context |
| `sync_turn(user, assistant, messages)` | detector pass → `store_memory`; tool-event scan feeds the error tracker and dedup buffer |
| `on_session_end(messages)` | flush error→resolution pairs, LLM extraction pass → `store_memory` + `finalize_session` |
| `on_memory_write(...)` | mirror built-in memory writes via `store_memory` |
| `get_tool_schemas()` / `handle_tool_call()` | native store/search/recall/reminder tools |
| `on_session_switch(reset=True)` | drop per-session detector state |

Capture design follows the pi companion (see
[`contrib/pi/extensions/alexandria/README.md`](../pi/extensions/alexandria/README.md)): heuristics
during the session, one LLM pass at session end, one shared dedup buffer, fail-open everywhere —
a down or slow Alexandria costs recall, never the conversation. Background work runs on Hermes'
`spawn_context_thread` so profile multiplexing keeps working.

## Tests

```sh
./run_tests.sh
```

Plain `unittest`, no pytest. The suite runs against `mock_server.py`, an in-memory MCP
StreamableHTTP server that speaks the same wire protocol as Alexandria (initialize handshake,
session header, JSON and SSE response bodies) with naive word-overlap scoring standing in for
embeddings. The mock is a protocol double, not a reference implementation of Alexandria's ranking.
The provider tests import Hermes' real `MemoryProvider` ABC when a Hermes source tree is present
and fall back to `tests/_hermes_stub/` otherwise (CI).

## Known gaps

- Not exercised against a live Alexandria in CI — the mock pins the wire contract, not ranking
  behaviour. A smoke test against a real server is worth adding if this gains users.
- The extraction pass uses whatever model the host has active; quality varies with it, and it is
  skipped entirely if the host exposes no plugin LLM.
- Heat/Ebbinghaus ranking is inert server-side (see
  [`docs/performance-and-ability-findings.md`](../../docs/performance-and-ability-findings.md) A2),
  so recall orders by cosine similarity only.
