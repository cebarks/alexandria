# Alexandria client-side integrations for Claude Code

Optional client-side pieces for Alexandria; they are not part of the MCP server and do not install automatically. The memory skill guides tool use. The hooks provide automatic recall, but do not automatically store observations or extract transcripts.

## Memory skill

The skill uses Claude Code's native MCP tool names (`mcp__alexandria__<tool>`) and provides explicit guidance for recalling, storing, updating, importing, and managing reminders.

```bash
cp -r contrib/claude/skills/alexandria-memory ~/.claude/skills/
```

For project-specific work, first resolve the project using `resolve_project` and pass its issued project ID. If there is no explicit project ID, the Claude recall hook is Personal-only. If you previously relied on regex-based prompt writes or transcript extraction, call `store_memory` explicitly for each durable observation (or `import_document` for a document) using the skill guidance.

## Hooks

The optional `alexandria-recall.sh` hook runs on `UserPromptSubmit`. It authenticates with `ALEXANDRIA_API_TOKEN`, resolves `ALEXANDRIA_PROJECT_ID` if supplied, and retrieves relevant memories. Without the override, it sends a Personal-scope request; it does not inspect or normalize git remotes. If project resolution or authentication fails, it refuses the operation rather than retrying unscoped. The hook does not write memories from correction/preference phrasing.

The `alexandria-session.sh` hook can add the Claude session ID and `agent_id: "claude-code"` to the agent's explicit `store_memory` and `import_document` calls. The former Stop-hook entrypoint, `alexandria-extract.sh`, is now a deprecated no-op retained only to give existing hook configurations an upgrade warning; remove it from Claude settings. Automatic transcript extraction and tool-error capture are no longer supported.

Install the active hooks:

```bash
ln -sf "$PWD"/contrib/claude/hooks/alexandria-{recall,session}.sh ~/.claude/hooks/
```

Add the prompt hook and optional explicit-store attribution hook to `~/.claude/settings.json` (merge with existing settings):

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "/home/you/.claude/hooks/alexandria-recall.sh" }] }
    ],
    "PreToolUse": [
      {
        "matcher": "mcp__alexandria__(store_memory|import_document)",
        "hooks": [{ "type": "command", "command": "/home/you/.claude/hooks/alexandria-session.sh" }]
      }
    ]
  }
}
```

Set `ALEXANDRIA_API_TOKEN` and optional `ALEXANDRIA_URL` / `ALEXANDRIA_PROJECT_ID` in the Claude hook environment (for example, the relevant settings `env` block). The token is the hook's bearer credential; it is separate from model-provider credentials and is not forwarded to a provider. A native MCP client connection is configured separately and must also supply an authorized bearer credential when connecting to an authenticated server.

The hook requires `bash`, `curl` 8 or later, `jq`, and `setsid`, `timeout`, `find`, `mktemp`, and GNU `stat -c %s`. The recall hook has a fixed 8-second overall budget and at most a 5-second individual HTTP request timeout. It does not read the Pi companion's TOML `[transport]` settings or `ALEXANDRIA_PROMPT_*` environment variables; those configurable timeouts apply to the Pi companion only.

Configuration:

| Variable | Default | Purpose |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | MCP server endpoint |
| `ALEXANDRIA_API_TOKEN` | unset (required) | Bearer token for hook MCP requests |
| `ALEXANDRIA_PROJECT_ID` | unset | Existing owned project handle; resolved before project-scoped recall |
| `ALEXANDRIA_AUTO_RECALL` | enabled | Set to `off` to disable recall |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `10` | Maximum results per prompt |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.45` | Injection threshold, measured with limit 10 |
| `ALEXANDRIA_AUTO_STORE` / `ALEXANDRIA_EXTRACT_*` | obsolete | No longer configure writers; remove them and the old Stop hook |

The hook uses `resolve_project` only for the explicit `ALEXANDRIA_PROJECT_ID` override. No override means Personal-only; unlike the Pi companion, the Claude hook has no git-remote descriptor resolution. It does not silently widen a failed project-scoped operation.

The hooks fail open with an availability message when recall cannot complete; they do not block the prompt. The deprecated extraction hook consumes its input and exits without calling the server. The helper can still make a deliberate one-shot MCP call for diagnostics or explicit use:

```bash
ALEXANDRIA_API_TOKEN=... ALEXANDRIA_URL=http://127.0.0.1:3000/mcp \
  contrib/claude/hooks/alexandria-recall.sh retrieve_memories '{"query":"which database","limit":3}'
```

The hermetic hook suite uses a local mock server and token fixture:

```bash
python3 -B -m unittest discover -s contrib/claude/tests -p 'test_*.py'
```
