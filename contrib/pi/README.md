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
| Time side | — | Calls `check_reminders` per prompt, so reminders actually arrive |
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

## `extensions/alexandria/`

This is a recall, store, **and** reminders companion (package version 2.1) — formerly
`extensions/alexandria-auto-recall/`. Five things happen:

1. **Auto-recall** — on `before_agent_start`, the user's prompt is embedded and searched; hits at or
   above `recall.min_similarity` are injected as a custom context message before the agent reasons.
2. **Reminder delivery** — in the same `before_agent_start` handler, `check_reminders` is called once
   per prompt and whatever is due is injected alongside the recall hits and shown as a notification.
   The server runs no timer, so this per-interaction check is how reminders reach the user at all.
3. **Heuristic detectors** — pattern matching on prompts and tool results, with no LLM in the loop:
   correction-shaped language ("no, use X"), forward-looking preference statements ("always do X"),
   and error→success pairs per tool, which become "this error resolves this way" memories. Stores are
   fire-and-forget so they never add latency to the turn.
4. **Dedup tracking** — every heuristic store, and every agent-initiated `store_memory` /
   `update_memory` the extension observes on the way past `tool_result`, is buffered so the
   extraction pass below can be told what is already saved.
5. **LLM extraction** — on `session_shutdown`, the conversation is serialized and a cheap model
   (`store.extract_model`) extracts durable facts that layers 3–4 missed. Skipped on `reload`, since
   that is not a real conversation boundary.

Layers 1–2 share one `before_agent_start` handler and each fail on their own, so a broken reminder
check never suppresses recall or vice versa. This mirrors the server's own design intent: the skill is
highest-quality but depends on the agent choosing to act, heuristics are zero-latency but shallow, and
extraction is thorough but arrives only at the end.

### Requires `npm install`

It depends on `@modelcontextprotocol/client` and `smol-toml`. pi's extension loader (jiti) does not
alias third-party npm packages the way it does pi's own internal packages, so a bare `.ts` file
cannot resolve them — this has to be a package-style extension directory with its own
`node_modules`.

**Install:**

```bash
cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria
npm install
```

The directory used to be `extensions/alexandria-auto-recall/`. If the old copy is still in
`~/.pi/agent/extensions/`, delete it after installing the new one, or pi loads both.

### Configuration

Config file: `$XDG_CONFIG_HOME/alexandria/client.toml`, same precedence as the server
(defaults → file → `ALEXANDRIA_CLIENT_CONFIG` → individual env vars). See
[docs/configuration.md](../../docs/configuration.md) for the full reference. Every key is optional;
the defaults work against a locally running server. Note: the extension default
`min_similarity = 0.58` is measured too high for `all-MiniLM-L6-v2` — set `0.35`
(see `[recall]` in docs/configuration.md).

```toml
[server]
url = "http://127.0.0.1:3000/mcp"

[recall]
enabled = true
limit = 5
min_similarity = 0.58

[store]
enabled = true
extract_model = "vertex/claude-haiku-4-5"
extract_timeout_ms = 5000

[reminders]
enabled = true
project = "alexandria"
```

Env vars cover the same ground (`ALEXANDRIA_REMINDERS=off`, `ALEXANDRIA_REMINDERS_PROJECT`, …); see
[`extensions/alexandria/README.md`](extensions/alexandria/README.md) for the full list, the
`client.toml` shape, and how reminder delivery, project targeting, and overdue escalation work.

### Failure behavior

The extension **fails open** in every path: if the server is unreachable or a call errors, the agent
turn proceeds normally with a warning notification, and extraction failures are logged and skipped.
A stopped Alexandria server never blocks a pi session.

Stale Streamable HTTP sessions (typically an Alexandria restart) are recovered transparently — the
client detects "Session not found", reconnects, and retries once before surfacing an error. The
reminder check is fail-open the same way: a missing `git`, an unreachable server, or a malformed
response produces a warning at most, never a blocked turn.

### Limitations worth knowing

- **No unit tests.** The detector and extraction-prompt logic has no test coverage in the repo, even
  though the original design called for one. Regex changes are currently unguarded.
- **No session memory integration.** The extension stores and retrieves without a `session_id`, so
  its writes are ungrouped. See [docs/session-memory.md](../../docs/session-memory.md).
- **Recall ignores `recall`.** It uses `retrieve_memories`, never the two-phase `recall` tool, so
  broad "what do we know about X" exploration is not what auto-recall is tuned for.
- **Reminders need a client to ask.** Nothing fires on its own: with `ALEXANDRIA_REMINDERS=off` (or
  no companion extension) reminders reach the user only if the agent calls `check_reminders`.
- **Extraction is end-of-session and best-effort.** It reads the tail of a long conversation (capped
  by character budget) and can time out; treat it as a safety net, not a transcript index.
