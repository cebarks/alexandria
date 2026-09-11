# Alexandria Auto-Recall & Auto-Store Extension

Pi extension that recalls relevant memories before each agent turn and stores durable facts both
during and at the end of a session.

The full guide — how this compares to the `alexandria-memory` skill, when you want it at all, and
how to install it — is in [`contrib/pi/README.md`](../../README.md). This file is the
in-directory reference for behavior and configuration.

## What it does

| Layer | Hook | Behavior |
| --- | --- | --- |
| Auto-recall | `before_agent_start` | Embeds the user's prompt, calls `retrieve_memories`, injects hits with `similarity >= recall.min_similarity` as an `alexandria-auto-recall` context message. The injected block tells the agent to verify relevance and to `update_memory` anything stale. |
| Correction detector | `before_agent_start` | Regex over the prompt for correction-shaped language ("no, use X"). Fires only on unambiguous matches; ambiguous cases are left to the extraction pass. |
| Preference detector | `before_agent_start` | Regex for forward-looking preference/convention statements ("always do X"). |
| Error tracker | `tool_execution_end` → `agent_end` | Pairs a failing tool call with a later success of the same tool and stores the resolution. Errors must contain a recognized signal to be tracked. |
| Dedup tracker | `tool_result` | Records content from agent-initiated `store_memory` / `update_memory` calls (matched by tool-name suffix, so the MCP prefix doesn't matter) so extraction doesn't re-report them. |
| LLM extraction | `session_shutdown` | Serializes the conversation, sends it to `store.extract_model`, stores what's left tagged `extracted`. Skipped when the shutdown reason is `reload`. |

Heuristic stores are fire-and-forget and never block a turn. Only the extraction pass can add
latency, and only at session end.

## Configuration

Config file: `$XDG_CONFIG_HOME/alexandria/client.toml` (`~/Library/Application Support/alexandria/client.toml`
on macOS), overridable with `ALEXANDRIA_CLIENT_CONFIG`. Environment variables win over the file,
which wins over defaults. Unparseable TOML warns and falls back to defaults rather than failing.

```toml
[server]
url = "http://127.0.0.1:3000/mcp"

[recall]
enabled = true
limit = 5
min_similarity = 0.35

[store]
enabled = true
extract_model = "vertex/claude-haiku-4-5"
extract_timeout_ms = 5000
```

### Environment variables

| Variable | Default | Description |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria server MCP endpoint |
| `ALEXANDRIA_AUTO_RECALL` | (enabled) | Set to `off` to disable auto-recall |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories to retrieve |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.35` | Minimum cosine similarity, inclusive (model-dependent; sits above the server-side `[retrieve] min_similarity` floor). Measured on the live corpus for `all-MiniLM-L6-v2`; the former `0.58` default delivered 4 of 12 known targets against `0.35`'s 9. See `[recall]` in `docs/configuration.md` |
| `ALEXANDRIA_AUTO_STORE` | (enabled) | Set to `off` to disable all store behavior — detectors and extraction alike |
| `ALEXANDRIA_EXTRACT_MODEL` | `vertex/claude-haiku-4-5` | Model for the LLM extraction pass |
| `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | `5000` | Extraction timeout in milliseconds |
| `ALEXANDRIA_CLIENT_CONFIG` | (XDG default) | Path to an alternate client TOML config |

`enabled = false` in the TOML file disables a layer the same way `off` does in the environment,
except an explicit env var always overrides the file.

## Failure behavior

Fails **open** everywhere: an unreachable server, a failed retrieval, or a failed extraction
produces a warning notification and a normal agent turn, never a blocked one. Stale Streamable HTTP
sessions — usually an Alexandria restart — are detected and retried once on a fresh connection.

Extraction routes through pi's `ctx.modelRegistry`, so provider auth (Vertex OAuth, Anthropic keys)
is handled by pi rather than by this extension. If the configured model or provider is unavailable,
it falls back to the session's own model.

## Known gaps

- No unit tests for the detectors or the extraction prompt.
- Does not pass `session_id`, so its memories are not grouped into Alexandria sessions.
- Uses `retrieve_memories` only; the two-phase `recall` tool is never called.

See [`contrib/pi/README.md`](../../README.md) for the full limitation list.
