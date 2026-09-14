# Alexandria Companion Extension

Pi extension that puts Alexandria in the loop without waiting for the agent to decide: it recalls
relevant memories and delivers due reminders before each turn, and stores durable facts it spots in
the conversation or extracts at session end.

Formerly `extensions/alexandria-auto-recall/`. The directory name (and therefore the install path)
changed, as did the client name and version the extension reports to the server (`alexandria`
`2.1.0`, which is what shows up in server logs and client-info); `client.toml`, the env vars, and
the server side did not. If you installed it by copying, remove
`~/.pi/agent/extensions/alexandria-auto-recall` before installing the new directory, or pi will load
both.

## Features

| Feature | What it does | Disable with |
| --- | --- | --- |
| Recall | Calls `retrieve_memories` on every prompt and injects hits above a similarity threshold into context. | `ALEXANDRIA_AUTO_RECALL=off` or `[recall] enabled = false` |
| Store | Heuristic detectors for corrections, preferences, and error resolutions, plus an LLM extraction pass at session shutdown. | `ALEXANDRIA_AUTO_STORE=off` or `[store] enabled = false` |
| Reminders | Calls `check_reminders` once per prompt and injects whatever is due. | `ALEXANDRIA_REMINDERS=off` or `[reminders] enabled = false` |

Recall and reminders run concurrently in a single `before_agent_start` handler and merge into one
injected message (`customType: "alexandria"`). Each feature fails on its own: a reminder check that
throws still lets recall inject, and vice versa. The store detectors are separate handlers and are
unaffected by the recall/reminders toggles.

**Install:**

```bash
cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria
npm install
```

## Reminders

The server runs no timer. A reminder becomes visible only when a client calls `check_reminders`, so
this extension is the delivery path: it checks once per prompt, before the agent starts.

- **Delivered once.** `check_reminders` consumes what it returns, so a reminder appears exactly once
  — the agent has to act on it or tell the user, because nothing will raise it again. Recurring
  schedules advance to their next fire rather than being consumed (one that has no future fire left
  is consumed on its last delivery).
- **Missed fires coalesce.** If a daily reminder was due three times while nothing checked, the next
  check delivers it once and labels it `(missed 3 earlier occurrence(s))` instead of sending three
  copies.
- **Project targeting.** Each check sends a project hint: `ALEXANDRIA_REMINDERS_PROJECT` if set,
  otherwise the basename of `git rev-parse --show-toplevel`, otherwise nothing. Reminders targeted at
  that project are delivered; global ones are delivered anywhere. The match is exact and
  case-sensitive against the `target_project` passed to `set_reminder`.
- **Escalation.** A project-targeted reminder that has been overdue longer than the server's
  `[reminders] escalation_hours` (default 48) is delivered regardless of the hint, labeled
  `[OVERDUE — escalated from project targeting]`. So a project you stop visiting cannot silently keep
  its reminders unsurfaced.
- **Two audiences.** Each delivery is both injected into context (the agent-visible half) and shown
  as `⏰ N Alexandria reminder(s) due` (the human-visible half), so a delivery can't be swallowed
  without the user noticing.

Set `ALEXANDRIA_REMINDERS_PROJECT` when the checkout directory is not the project name — most often
in a **git worktree**, where the toplevel basename is the worktree directory (`feature/reminders`)
rather than the repository (`alexandria`). Without the override, reminders targeted at the real
project name arrive late and escalated rather than in their own project.

Everything here is fail-open: an unreachable server, a missing `git`, or a malformed response
produces a warning notification at most, never a blocked turn. A check that gives up at the client
(5 s) may already have consumed its rows server-side, so that one fire is lost; a malformed or
error-shaped response consumes nothing, warns once, and is retried on the next prompt.

## Configuration

Config file: `$XDG_CONFIG_HOME/alexandria/client.toml`

Override with `ALEXANDRIA_CLIENT_CONFIG` env var, or individual `ALEXANDRIA_*` env vars. Env values
win over the file; `enabled = false` in the file is honored only when the matching env var is unset.

See [docs/configuration.md](../../../../docs/configuration.md) for the full reference.

### Example `client.toml`

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

### Environment Variables

| Variable | Default | Description |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria server MCP endpoint |
| `ALEXANDRIA_AUTO_RECALL` | (enabled) | Set to `off` to disable auto-recall |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories to retrieve |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.58` | Minimum cosine similarity threshold (model-dependent; sits above the server-side `[retrieve] min_similarity` floor) |
| `ALEXANDRIA_AUTO_STORE` | (enabled) | Set to `off` to disable all auto-store |
| `ALEXANDRIA_EXTRACT_MODEL` | `vertex/claude-haiku-4-5` | Model for LLM extraction pass |
| `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | `5000` | Extraction timeout in milliseconds |
| `ALEXANDRIA_REMINDERS` | (enabled) | Set to `off` to disable per-prompt reminder checks |
| `ALEXANDRIA_REMINDERS_PROJECT` | (git repo dir name) | Project hint sent to `check_reminders`, for exact matching against reminder targets |
| `ALEXANDRIA_CLIENT_CONFIG` | (XDG default) | Path to alternate client TOML config |

`ALEXANDRIA_AUTO_RECALL` is the original name of the recall toggle and is still the spelling it
uses; `ALEXANDRIA_REMINDERS` follows the same shape for the reminders feature.
