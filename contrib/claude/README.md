# Claude Code client-side integrations

Optional companions for using Alexandria from [Claude Code](https://claude.com/claude-code).
Not part of the MCP server; they exist to make the agent reach for memory more proactively.
See "Getting agents to actually use memory" in the root [README.md](../../README.md).

Nothing here auto-installs.

## `skills/alexandria-memory/`

Same guidance as the Pi skill, using Claude Code's `mcp__alexandria__<tool>` names.

```bash
cp -r contrib/claude/skills/alexandria-memory ~/.claude/skills/
```

## `hooks/alexandria-recall.sh`

A `UserPromptSubmit` hook that calls `retrieve_memories` on every prompt and prints hits above a
similarity threshold to stdout, which Claude Code appends to context. It also prints the Claude
Code `session_id` so the agent can pass it to `store_memory` and group memories per session.
Equivalent of the Pi auto-recall extension, minus auto-store. Needs only `bash`, `curl` ≥ 8 and `jq`.

Fails open: if the server is unreachable or errors, the prompt proceeds with nothing injected
(a one-line note goes to stderr).

**Install:**

```bash
cp contrib/claude/hooks/alexandria-recall.sh ~/.claude/hooks/
```

Then add to `~/.claude/settings.json` (merge with any existing `hooks` block):

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          { "type": "command", "command": "/home/you/.claude/hooks/alexandria-recall.sh" }
        ]
      }
    ]
  }
}
```

**Config (env vars, all optional):**

| Variable | Default | Purpose |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria MCP server URL |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories retrieved per prompt |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.15` | Minimum similarity to inject a hit (see `[retrieve]` in [docs/configuration.md](../../docs/configuration.md) for why this is low) |
| `ALEXANDRIA_AUTO_RECALL` | (unset) | Set to `off` to disable |

Set them in the hook command itself if needed, e.g.
`"command": "ALEXANDRIA_AUTO_RECALL_LIMIT=3 /home/you/.claude/hooks/alexandria-recall.sh"`.

**Debugging:** the script doubles as a one-shot MCP tool caller:

```bash
contrib/claude/hooks/alexandria-recall.sh retrieve_memories '{"query":"which database","limit":3}'
```

`hooks/test.sh` is a manual end-to-end check against a running server
(`ALEXANDRIA_URL=... contrib/claude/hooks/test.sh`).
