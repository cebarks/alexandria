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

## `hooks/`

Equivalent of the Pi auto-recall extension. Needs only `bash`, `curl` ≥ 8, `jq`, and (for
extraction) the `claude` CLI.

`alexandria-recall.sh` is a `UserPromptSubmit` hook. On every prompt it opens one MCP session and:

- calls `retrieve_memories` and returns hits above a similarity threshold as `additionalContext`,
  which Claude Code appends to the prompt;
- scans the prompt for correction ("no, use X", "that's wrong, ...", "actually ...") and preference
  ("always ...", "never ...", "from now on ...", "use X instead of Y") phrasing, same patterns as the
  Pi detectors, and stores unambiguous hits as `User correction: ...` / `User preference: ...` with
  tags `correction`/`preference` + `auto-detected` and the session id. Deduped per session via
  `$XDG_STATE_HOME/alexandria/<session_id>.stored`. The Pi error-resolution tracker is not ported;
  failed tool results are fed to the extraction pass instead (below).

`alexandria-extract.sh` is a `Stop` hook. After each assistant turn it serializes the transcript lines
added since its last run (user text, assistant text, and the first 300 characters of each failed
tool result as `[Tool error]: <tool name> <its command or file path, else the first 120 chars of its input> -- <error>`, so a silent
fix-and-retry still shows the model the root cause and which call produced it;
successful tool output, `<tool_use_error>` harness refusals, thinking, and injected system lines are
dropped), and once at least `ALEXANDRIA_EXTRACT_MIN_CHARS` of new text exists it
asks `claude -p --model haiku` for standalone durable facts using the Pi extraction prompt, with the
session's already-stored memories and the auto-recall hits the transcript carries for this chunk's
prompts listed for dedup (so a gotcha already stored by an earlier session is not stored again, as long
as some prompt recalled it). Results are stored with the session id and an
`extracted` tag. Short turns cost nothing; one haiku call covers several turns. A marker file
`$XDG_STATE_HOME/alexandria/<session_id>.extracted` holds the transcript line count and is written
before the LLM call, so a failed or slow turn is never retried: one haiku call per turn, 80 s
timeout. The child `claude` runs with
`ALEXANDRIA_HOOK_CHILD=1`, which makes every hook here exit immediately (no recursion). Measured
2026-09-08 on a ~40-line transcript: about 15 s wall time, haiku correctly returned no memories for a
purely tactical session.

`alexandria-session.sh` is a `PreToolUse` hook matched on `mcp__alexandria__store_memory` and
`mcp__alexandria__import_document`. When the agent calls either without a `session_id` or
`agent_id`, it rewrites the call to include the Claude Code session id and `agent_id: "claude-code"`,
so memories and imported chunks are grouped per session and attributed without relying on the model
to remember. The recall and extract hooks stamp the same `agent_id` on the stores they make themselves.

All three fail open. If the server is unreachable or errors, the recall hook returns a `systemMessage`
("Alexandria memory unavailable: ...") so you can see it, and the prompt proceeds with nothing
injected. The session hook never blocks a tool call. The extract hook logs one line to stderr and
exits 0.

**Install:**

```bash
ln -sf "$PWD"/contrib/claude/hooks/alexandria-{recall,session,extract}.sh ~/.claude/hooks/
```

Symlinks, not copies: the installed hooks then track the repo, and the extract hook resolves its
sibling through `readlink -f`, so it still finds `alexandria-recall.sh` next to the real file.

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
        "matcher": "mcp__alexandria__(store_memory|import_document)",
        "hooks": [
          { "type": "command", "command": "/home/you/.claude/hooks/alexandria-session.sh" }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          { "type": "command", "command": "/home/you/.claude/hooks/alexandria-extract.sh" }
        ]
      }
    ]
  }
}
```

The extract hook must live under `Stop`, not `SessionEnd`: `SessionEnd` hooks share a 1.5 s budget,
far too short for an LLM call. Stop hooks block the next turn until they exit, and Claude Code
kills hooks still running when the session ends, so the script re-execs itself with `setsid -f`
(own session and process group, no inherited pipes) and returns in milliseconds; the detached copy
does the 15–80 s LLM call, never holds your next turn, and finishes even if you quit right after
your last turn (verified 2026-09-08: a 10 s stub completed 11 s after the headless session exited).
The script's own 80 s budget bounds a wedged `claude -p`. No `"async": true` is needed. Its stderr
goes to `$XDG_STATE_HOME/alexandria/extract.log` (default `~/.local/state/alexandria/`), rotated to
`extract.log.1` once it passes 1 MiB; marker files in the same directory idle for over
`ALEXANDRIA_MARKER_MAX_AGE_DAYS` days (default 7) are pruned at the same time.

**Config (env vars, all optional):**

| Variable | Default | Purpose |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria MCP server URL |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories retrieved per prompt |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.35` | Minimum similarity to inject a hit (measured; see `[recall]` in [docs/configuration.md](../../docs/configuration.md)) |
| `ALEXANDRIA_AUTO_RECALL` | (unset) | Set to `off` to disable recall |
| `ALEXANDRIA_AUTO_STORE` | (unset) | Set to `off` to disable the detectors and extraction. Headless sessions (`claude -p`, Agent SDK; `CLAUDE_CODE_ENTRYPOINT=sdk-*`) default to off so scripted experiments never land in the real database; set `on` to enable there |
| `ALEXANDRIA_EXTRACT_MODEL` | `haiku` | Model passed to `claude -p --model` for extraction |
| `ALEXANDRIA_EXTRACT_MIN_CHARS` | `1500` | New transcript text required before an extraction call |
| `ALEXANDRIA_EXTRACT_FLUSH_WAIT` | `1` | Seconds to wait before reading the transcript; Stop fires ~50 ms before the last assistant message is flushed (tests set `0`) |
| `ALEXANDRIA_EXTRACT_CMD` | (unset) | Replace the `claude -p ...` command (prompt on stdin, JSON on stdout); used by tests |
| `ALEXANDRIA_MARKER_MAX_AGE_DAYS` | `7` | Per-session marker files idle longer than this are pruned by the extract hook |
| `ALEXANDRIA_HOOK_CHILD` | (unset) | Set by the extract hook on its `claude -p` child; every hook exits immediately when set |
| `ALEXANDRIA_DETACHED` | (unset) | Set by the extract hook on its detached copy; set it yourself to run the hook inline (tests do) |

`CLAUDE_CODE_ENTRYPOINT` is an internal Claude Code variable, not in the documented settings list. The
`sdk-*` gate matches the binary's own "running under an SDK" check (`sdk-cli` for `claude -p`, `sdk-ts` and
`sdk-py` for the Agent SDKs; confirmed on 2.1.263 from the bundled JS and the Python SDK source; interactive
is `cli`). If a release renames it, the hooks silently fall back to always-on. After upgrading, re-check with

```bash
claude -p 'Run with the Bash tool and reply with only its output: echo ENTRYPOINT=$CLAUDE_CODE_ENTRYPOINT' --allowedTools Bash
```

which prints the value the hooks see (a Bash tool call inherits the env; no stub needed). The prompt
must come before `--allowedTools`, which otherwise swallows it as a tool name.

The hooks are configured by env vars only; they do not read `client.toml` (bash has no TOML parser,
and a `yq`/`tomlq` dependency for a handful of values is worse than a handful of env vars). Set them
in the `env` block of `~/.claude/settings.json`, or in the hook command itself, e.g.
`"command": "ALEXANDRIA_AUTO_RECALL_LIMIT=3 /home/you/.claude/hooks/alexandria-recall.sh"`.

**Debugging:** the script doubles as a one-shot MCP tool caller:

```bash
contrib/claude/hooks/alexandria-recall.sh retrieve_memories '{"query":"which database","limit":3}'
```

`hooks/test.sh` is a manual end-to-end check against a running server
(`ALEXANDRIA_URL=... contrib/claude/hooks/test.sh`).
