# pi client-side integrations

These are optional client-side companions for using Alexandria from
[pi](https://github.com/earendil-works/pi-mono). They are not part of the MCP server and are not
required to use Alexandria — they exist purely to make pi agents reach for memory more
proactively. See the "Getting agents to actually use memory" section in the root
[README.md](../../README.md) for the full rationale.

Nothing here auto-installs. Copy what you want into your pi config directory.

## `skills/alexandria-memory/`

A pi [skill](https://github.com/earendil-works/pi-mono/blob/main/packages/coding-agent/docs/skills.md)
documenting concrete trigger conditions for when an agent should read/write memory and which tool
to use. Skills are prose guidance loaded into the agent's context — no code, no dependencies.

**Install:**

```bash
cp -r contrib/pi/skills/alexandria-memory ~/.pi/agent/skills/
```

(Or `.pi/skills/` for a project-local install. See pi's skill docs for all discovery locations.)

## `extensions/alexandria/`

A pi extension that puts Alexandria in the loop on every prompt, with three independently-toggled
features:

- **Recall** — hooks `before_agent_start` to call `retrieve_memories` and inject matches above a
  similarity threshold into context, so the agent never has to decide to check memory.
- **Store** — heuristic detectors for corrections, preferences, and error resolutions, plus an LLM
  extraction pass at session shutdown.
- **Reminders** — calls `check_reminders` once per prompt and injects whatever is due. The server
  runs no timer, so a per-interaction check is how reminders reach the user at all.

Recall and reminders share one `before_agent_start` handler and each fail on their own. This is the
more aggressive nudge (Tier 3 in the root README): it trades latency (a recall round trip, a
reminder check, and a git lookup on the first prompt in a session, run concurrently) and potential
noise for guaranteed recall. Prefer the skill alone unless you find agents still aren't checking
memory often enough.

Requires an npm install because it depends on `@modelcontextprotocol/client` — pi's extension
loader (jiti) doesn't alias third-party npm packages the way it does pi's own internal packages,
so a bare `.ts` file can't resolve this dependency. It has to be a package-style extension
directory with its own `node_modules`.

**Install:**

```bash
cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria
npm install
```

This directory used to be `extensions/alexandria-auto-recall/`. If the old copy is still in
`~/.pi/agent/extensions/`, delete it after installing the new one, or pi loads both.

**Config (env vars, all optional — each feature also has an `enabled` key in `client.toml`):**

| Variable | Default | Purpose |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria MCP server URL |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories retrieved per prompt |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.58` | Minimum similarity to inject a hit |
| `ALEXANDRIA_AUTO_RECALL` | (unset) | Set to `off` to disable recall |
| `ALEXANDRIA_AUTO_STORE` | (unset) | Set to `off` to disable all store behavior |
| `ALEXANDRIA_REMINDERS` | (unset) | Set to `off` to disable per-prompt reminder checks |
| `ALEXANDRIA_REMINDERS_PROJECT` | (git repo dir name) | Project hint sent with the reminder check |

See [`extensions/alexandria/README.md`](extensions/alexandria/README.md) for the full list, the
`client.toml` shape, and how reminder delivery and escalation work.

The extension fails open: if the Alexandria server is unreachable or errors, the agent turn
proceeds normally with a warning notification, never blocked.
