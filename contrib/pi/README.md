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

## `extensions/alexandria-auto-recall/`

A pi extension that hooks `before_agent_start` to automatically call `retrieve_memories` on every
user prompt and inject matches above a similarity threshold into context — so the agent never has
to decide to check memory. This is the more aggressive nudge (Tier 3 in the root README): it trades
latency (one extra HTTP + embedding round trip per prompt) and potential noise for guaranteed
recall. Prefer the skill alone unless you find agents still aren't checking memory often enough.

Requires an npm install because it depends on `@modelcontextprotocol/client` — pi's extension
loader (jiti) doesn't alias third-party npm packages the way it does pi's own internal packages,
so a bare `.ts` file can't resolve this dependency. It has to be a package-style extension
directory with its own `node_modules`.

**Install:**

```bash
cp -r contrib/pi/extensions/alexandria-auto-recall ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria-auto-recall
npm install
```

**Config (env vars, all optional):**

| Variable | Default | Purpose |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria MCP server URL |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories retrieved per prompt |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.5` | Minimum similarity to inject a hit |
| `ALEXANDRIA_AUTO_RECALL` | (unset) | Set to `off` to disable |

The extension fails open: if the Alexandria server is unreachable or errors, the agent turn
proceeds normally with a warning notification, never blocked.
