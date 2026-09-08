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

## `hooks/alexandria-recall.sh` and `hooks/alexandria-session.sh`

`alexandria-recall.sh` is a `UserPromptSubmit` hook that calls `retrieve_memories` on every prompt
and returns hits above a similarity threshold as `additionalContext`, which Claude Code appends to
the prompt. Equivalent of the Pi auto-recall extension, minus auto-store. Needs only `bash`,
`curl` ≥ 8 and `jq`.

`alexandria-session.sh` is a `PreToolUse` hook matched on `mcp__alexandria__store_memory`. When
the agent calls `store_memory` without a `session_id`, it rewrites the call to include the Claude
Code session id, so memories are grouped per session without relying on the model to remember.

Both fail open. If the server is unreachable or errors, the recall hook returns a `systemMessage`
("Alexandria memory unavailable: ...") so you can see it, and the prompt proceeds with nothing
injected. The session hook never blocks a tool call.

**Install:**

```bash
cp contrib/claude/hooks/alexandria-recall.sh contrib/claude/hooks/alexandria-session.sh ~/.claude/hooks/
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
    ],
    "PreToolUse": [
      {
        "matcher": "mcp__alexandria__store_memory",
        "hooks": [
          { "type": "command", "command": "/home/you/.claude/hooks/alexandria-session.sh" }
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
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.35` | Minimum similarity to inject a hit (measured; see `[recall]` in [docs/configuration.md](../../docs/configuration.md)) |
| `ALEXANDRIA_AUTO_RECALL` | (unset) | Set to `off` to disable |
| `ALEXANDRIA_HOOK_CHILD` | (unset) | Set by hooks that shell out to `claude -p`; both hooks exit immediately when set |

Set them in the hook command itself if needed, e.g.
`"command": "ALEXANDRIA_AUTO_RECALL_LIMIT=3 /home/you/.claude/hooks/alexandria-recall.sh"`.

**Debugging:** the script doubles as a one-shot MCP tool caller:

```bash
contrib/claude/hooks/alexandria-recall.sh retrieve_memories '{"query":"which database","limit":3}'
```

`hooks/test.sh` is a manual end-to-end check against a running server
(`ALEXANDRIA_URL=... contrib/claude/hooks/test.sh`).
