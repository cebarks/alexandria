# Alexandria Companion Extension

Pi extension for prompt-time memory recall and reminder checks. It does not automatically store observed corrections, preferences, tool-error resolutions, or session-end extractions. Save durable observations through an explicit authenticated `store_memory` call (or use `import_document` for a document), following the Alexandria memory skill.

Former installs used `extensions/alexandria-auto-recall/`; remove that old directory after
installing this one to avoid loading both.

## What it does

- Before each prompt, retrieves relevant memories and injects hits at or above the configurable
  similarity floor. The server embeds the prompt; the extension has no embedding dependency.
- In the same prompt path, checks due reminders and injects them alongside recall; Pi's UI
  notification is best-effort. Recall and reminders can be disabled independently.
- Does not call `store_memory`, import transcripts, or finalize sessions automatically. Explicit
  agent calls to authenticated MCP tools remain available.

Install:

```bash
cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria
npm install --omit=dev --ignore-scripts
```

## Authentication and project context

Set `ALEXANDRIA_API_TOKEN` to an issued Alexandria agent token. This extension's credential is independent of pi's model-provider credentials and is never passed to a provider. Missing/invalid authentication, or a server without the expected bearer-auth and `resolve_project` surface, produces upgrade/configuration guidance. The extension does not retry anonymously or fall back to unscoped requests after a project-resolution failure.

For each prompt, the extension uses the session's `ctx.cwd` to ask git for the `origin` remote, stripping inherited `GIT_*` environment overrides from the git process. It sanitizes supported HTTPS/SSH remotes into a descriptor (normalized host, optional non-default port, repository path); userinfo, query, and fragment are discarded before the descriptor is sent. The server resolves that descriptor to an issued project ID. The resolved ID scopes recall and reminder checks and is included in the injected context. Results are cached by server/account/directory/descriptor; changing server or account clears the cache. The companion checks the resolved endpoint and bearer identity before dispatch and again before a stale-session retry. A change stops that pending operation rather than sending it with different credentials; it cannot revoke a request already sent.

Set `ALEXANDRIA_PROJECT_ID` to override remote discovery with an existing owned project handle. The override is still resolved and validated by the authenticated server; it does not grant ownership. If there is no override and no usable remote, the extension sends no project selector and recall is Personal-only. It never guesses from the checkout basename. A server/auth/resolution failure refuses the scoped operation instead of silently widening it.

## Configuration

Config file: `$XDG_CONFIG_HOME/alexandria/client.json` (`~/Library/Application Support/alexandria/client.json`
on macOS when `XDG_CONFIG_HOME` is unset), overridable with `ALEXANDRIA_CLIENT_CONFIG`.
Environment variables win over the selected file, which wins over defaults. Invalid JSON warns and
falls back to defaults at startup; the menu refuses to overwrite an invalid or unreadable file.

**Breaking change:** the companion no longer reads `client.toml`. Convert an existing client file to
JSON manually, retaining the relevant section/key names, and update `ALEXANDRIA_CLIENT_CONFIG` if it
points to a TOML path. There is no automatic migration. The server's `config.toml` is unchanged.

Run `/alexandria` in interactive Pi to view and edit auto-recall's enabled state, limit (1–100), and
minimum similarity (-1 to 1). Changes are saved to the selected JSON file and apply to subsequent
prompts in that Pi process. Other running processes keep their loaded settings until reloaded.
Each environment override is labeled with its effective and saved values; saving does not override
the environment. Neither authentication nor project context is edited by this menu.
The menu follows symlinks, preserves unrelated JSON values and ordinary mode bits, refuses read-only
files and untrusted directories, and serializes companion writers. It uses no external filesystem
metadata tools.
The atomic replacement does **not** preserve ownership, ACLs, extended attributes, special mode
bits, or inode flags; effective access can change even if the mode bits look unchanged. Edit the
JSON manually if those attributes matter. Linux menu writes are tested; the macOS path has not been
tested on a real Mac. Windows menu writes are currently refused.

A group-writable temporary file can be changed before publication by someone in its group, which can
be different from the original file's group. Use an owner-only config mode or directory if that risk
matters. The lock does not constrain ordinary editors: avoid external edits while a menu save is in
progress. If a crash leaves a `<config>.lock` file, verify that no other writer is active before removing it.
Writes are not `fsync`ed, so a successful save does not guarantee survival of sudden power loss.

See [docs/configuration.md](../../../../docs/configuration.md) for the full reference, including why
`limit` and `min_similarity` must be considered together.

### Example `client.json`

```json
{
  "server": { "url": "http://127.0.0.1:3000/mcp" },
  "recall": { "enabled": true, "limit": 10, "min_similarity": 0.45 },
  "reminders": { "enabled": true },
  "transport": { "prompt_call_timeout_ms": 5000, "prompt_budget_ms": 10000 }
}
```

| Setting | Default | Environment override | Meaning |
| --- | ---: | --- | --- |
| `server.url` | `http://127.0.0.1:3000/mcp` | `ALEXANDRIA_URL` | MCP endpoint |
| `recall.enabled` | `true` | `ALEXANDRIA_AUTO_RECALL=off` | Disable auto-recall |
| `recall.limit` | `10` | `ALEXANDRIA_AUTO_RECALL_LIMIT` | Maximum retrieved results |
| `recall.min_similarity` | `0.45` | `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | Client-side injection threshold, calibrated together with limit 10 |
| `reminders.enabled` | `true` | `ALEXANDRIA_REMINDERS=off` | Disable per-prompt checks |
| `transport.prompt_call_timeout_ms` | `5000` ms | `ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS` | Per-call limit in the automatic prompt path |
| `transport.prompt_budget_ms` | `10000` ms | `ALEXANDRIA_PROMPT_BUDGET_MS` | Shared deadline for project lookup, handshake, recall, reminders, and reconnect attempts |

Timeouts must be positive finite numbers; per-call timeout must not exceed the whole-path budget,
and the whole-path budget must not exceed `120000` ms. Invalid timeout values fail configuration
rather than clamp. Each stage shares that deadline. These settings govern automatic prompt work,
not explicit native MCP calls.

These are automatic-path budgets, not hosted latency measurements. The companion does not submit refinement work or approve classification; use the [owner UI](../../../../docs/ownership-and-refinement.md) for review.

The old automatic-storage settings are obsolete: `ALEXANDRIA_AUTO_STORE`, `ALEXANDRIA_EXTRACT_MODEL`, `ALEXANDRIA_EXTRACT_TIMEOUT_MS`, and legacy `store` extraction fields (`enabled`, `extract_model`, `extract_timeout_ms`) are ignored with a warning. Remove them and explicitly call `store_memory` or `import_document` when something should be saved.

## Reminders

The server does not run a reminder timer. This extension asks once per prompt, so no client means no automatic delivery. The project selector it sends is the issued ID produced by the project-resolution flow above, not a repository name or basename. The server validates ownership and targeting; a failed check is not retried as a global check. See the [memory skill](../../skills/alexandria-memory/SKILL.md) for reminder semantics and safe tool use.

## Failure behavior and limits

Recall and reminder handling fail open with a warning where possible: a server outage does not block the agent turn. Prompt work is cancellable and bounded by the shared deadline; cancellation is distinguished from transport failure, and connections are dropped only for transport failures. The extension prewarms its MCP connection at load to keep the normal initial handshake off the first prompt path. Diagnostics are appended to `$XDG_STATE_HOME/alexandria-companion/prompt-path.jsonl` (default `~/.local/state/alexandria-companion/`), asynchronously and without blocking the prompt path.

The extension uses `retrieve_memories` rather than the two-phase `recall` tool. It neither groups nor
finalizes an Alexandria session automatically.

See [`contrib/pi/README.md`](../../README.md) for the skill/extension comparison and installation options, and [docs/configuration.md](../../../../docs/configuration.md) for the wider configuration reference.
