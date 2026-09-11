# pi client-side integrations

Optional client-side companions for using Alexandria from
[pi](https://github.com/earendil-works/pi-mono). They are not part of the MCP server, are not
shipped with it, and are not required to use Alexandria — they exist purely to make pi agents reach
for memory more proactively. See the "Getting agents to actually use memory" section in the root
[README.md](../../README.md) for the full rationale.

Nothing here auto-installs. Copy what you want into your pi config directory.

## Which one do you want?

| | Skill | Extension |
| --- | --- | --- |
| What it is | Prose guidance in agent context | TypeScript extension with event hooks |
| Read side | Tells the agent when to call `retrieve_memories` | Calls it for you on every prompt |
| Write side | Tells the agent when to call `store_memory` | Heuristic detectors + session-end LLM extraction |
| Cost | Context tokens only | HTTP + embedding round trip per prompt, one LLM call per session |
| Install | Copy a directory | Copy a directory + `npm install` |

Start with the skill. Add the extension only if agents still aren't checking memory often enough,
or if you want capture to continue even when the agent forgets to write.

## `skills/alexandria-memory/`

A pi [skill](https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/docs/skills.md)
documenting concrete trigger conditions for when an agent should read or write memory and which tool
to pick. Skills are guidance loaded into the agent's context — no code, no dependencies.

The tool names in the skill are the MCP-prefixed forms pi exposes (`alexandria_store_memory`,
`alexandria_retrieve_memories`, …), not the bare server-side names.

**Install:**

```bash
cp -r contrib/pi/skills/alexandria-memory ~/.pi/agent/skills/
```

(Or `.pi/skills/` for a project-local install. See pi's skill docs for all discovery locations.)

## `extensions/alexandria-auto-recall/`

Despite the directory name — kept for install-path stability — this is a recall **and** store
extension (package version 2.0). Four things happen:

1. **Auto-recall** — on `before_agent_start`, the user's prompt is embedded and searched; hits at or
   above `recall.min_similarity` are injected as a custom context message before the agent reasons.
2. **Heuristic detectors** — pattern matching on prompts and tool results, with no LLM in the loop:
   correction-shaped language ("no, use X"), forward-looking preference statements ("always do X"),
   and error→success pairs per tool, which become "this error resolves this way" memories. Stores are
   fire-and-forget so they never add latency to the turn.
3. **Dedup tracking** — every heuristic store, and every agent-initiated `store_memory` /
   `update_memory` the extension observes on the way past `tool_result`, is buffered so the
   extraction pass below can be told what is already saved.
4. **LLM extraction** — on `session_shutdown`, the conversation is serialized and a cheap model
   (`store.extract_model`) extracts durable facts that layers 1–3 missed. Skipped on `reload`, since
   that is not a real conversation boundary.

This mirrors the server's own design intent: the skill is highest-quality but depends on the agent
choosing to act, heuristics are zero-latency but shallow, and extraction is thorough but arrives
only at the end.

### Requires `npm install`

It depends on `@modelcontextprotocol/client` and `smol-toml`. pi's extension loader (jiti) does not
alias third-party npm packages the way it does pi's own internal packages, so a bare `.ts` file
cannot resolve them — this has to be a package-style extension directory with its own
`node_modules`.

**Install:**

```bash
cp -r contrib/pi/extensions/alexandria-auto-recall ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria-auto-recall
npm install
```

### Configuration

Config file: `$XDG_CONFIG_HOME/alexandria/client.toml`, same precedence as the server
(defaults → file → `ALEXANDRIA_CLIENT_CONFIG` → individual env vars). See
[docs/configuration.md](../../docs/configuration.md) for the full reference. Every key is optional;
the defaults work against a locally running server.

`limit` and `min_similarity` default to `10` and `0.45`, measured together for
`all-MiniLM-L6-v2` (see `[recall]` in [docs/configuration.md](../../docs/configuration.md)).
They are one setting in two keys: a memory ranked outside `limit` cannot be recovered by any
threshold. The earlier `5`/`0.35` pair delivered the same 8 of 12 known targets at three times
the injected noise, and the `0.58` default before that delivered only 4.

```toml
[server]
url = "http://127.0.0.1:3000/mcp"

[recall]
enabled = true
limit = 10
min_similarity = 0.45

[store]
enabled = true
extract_model = "vertex/claude-haiku-4-5"
extract_timeout_ms = 5000
```

### Failure behavior

The extension **fails open** in every path: if the server is unreachable or a call errors, the agent
turn proceeds normally with a warning notification, and extraction failures are logged and skipped.
A stopped Alexandria server never blocks a pi session.

Stale Streamable HTTP sessions (typically an Alexandria restart) are recovered transparently — the
client detects "Session not found", reconnects, and retries once before surfacing an error.

### Limitations worth knowing

- **Only the pure logic is tested.** `just test-pi` (or `npm test` in the extension directory)
  runs `node:test` over the detectors, the dedup buffer, and the extraction serializer/parser.
  The extraction prompt itself, recall, and the MCP client have no coverage.
- **Sessions are grouped but never finalized.** Every auto-store write carries pi's session id as
  `session_id` (plus `agent_id="pi"` and the active model), so `list_sessions(agent_id="pi")` and
  `get_session` see them. The extension does not call `finalize_session`, and recall is not scoped
  to the session. See [docs/session-memory.md](../../docs/session-memory.md).
- **Recall ignores `recall`.** It uses `retrieve_memories`, never the two-phase `recall` tool, so
  broad "what do we know about X" exploration is not what auto-recall is tuned for.
- **Extraction is end-of-session and best-effort.** It reads the tail of a long conversation (capped
  by character budget) and can time out; treat it as a safety net, not a transcript index.
