# Alexandria Companion Extension

Pi extension for prompt-time memory recall and reminder checks. It does not automatically store observed corrections, preferences, tool-error resolutions, or session-end extractions. Save durable observations through an explicit authenticated `store_memory` call (or use `import_document` for a document), following the Alexandria memory skill.

Former installs used `extensions/alexandria-auto-recall/`; remove that old directory after installing this one to avoid loading both.

## What it does

- Before each prompt, retrieves relevant memories and injects qualifying results into the agent context.
- In the same prompt path, calls `check_reminders` and injects due reminders; pi's UI notification is best-effort.
- Does not make independent `store_memory` calls. Explicit agent calls to the MCP tools still work normally.

Install:

```bash
cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria
npm install --omit=dev --ignore-scripts
```

## Authentication and project context

Set `ALEXANDRIA_API_TOKEN` to an issued Alexandria agent token. This extension's credential is independent of pi's model-provider credentials and is never passed to a provider. Missing/invalid authentication, or a server without the expected bearer-auth and `resolve_project` surface, produces upgrade/configuration guidance. The extension does not retry anonymously or fall back to unscoped requests after a project-resolution failure.

For each prompt, the extension uses the session's `ctx.cwd` to ask git for the `origin` remote, stripping inherited `GIT_*` environment overrides from the git process. It sanitizes supported HTTPS/SSH remotes into a descriptor (normalized host, optional non-default port, repository path); userinfo, query, and fragment are discarded before the descriptor is sent. The server resolves that descriptor to an issued project ID. The resolved ID scopes recall and reminder checks and is included in the injected context. Results are cached by server/account/directory/descriptor; changing server or account clears the cache.

Set `ALEXANDRIA_PROJECT_ID` to override remote discovery with an existing owned project handle. The override is still resolved and validated by the authenticated server; it does not grant ownership. If there is no override and no usable remote, the extension sends no project selector and recall is Personal-only. It never guesses from the checkout basename. A server/auth/resolution failure refuses the scoped operation instead of silently widening it.

## Configuration

The extension reads `$XDG_CONFIG_HOME/alexandria/client.toml`; on macOS the default is `~/Library/Application Support/alexandria/client.toml`, unless `XDG_CONFIG_HOME` is set. `ALEXANDRIA_CLIENT_CONFIG` selects an alternate TOML file. Precedence is defaults → file → environment overrides. Invalid TOML warns and falls back to defaults.

```toml
[server]
url = "http://127.0.0.1:3000/mcp"

[recall]
enabled = true
limit = 10
min_similarity = 0.45

[reminders]
enabled = true

[transport]
prompt_call_timeout_ms = 5000
prompt_budget_ms = 10000
```

| Setting | Default | Environment override | Meaning |
| --- | ---: | --- | --- |
| `[server].url` | `http://127.0.0.1:3000/mcp` | `ALEXANDRIA_URL` | MCP endpoint |
| `[recall].enabled` | `true` | `ALEXANDRIA_AUTO_RECALL=off` | Disable auto-recall |
| `[recall].limit` | `10` | `ALEXANDRIA_AUTO_RECALL_LIMIT` | Maximum retrieved results |
| `[recall].min_similarity` | `0.45` | `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | Client-side injection threshold, calibrated together with limit 10 |
| `[reminders].enabled` | `true` | `ALEXANDRIA_REMINDERS=off` | Disable per-prompt checks |
| `[transport].prompt_call_timeout_ms` | `5000` ms | `ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS` | Per-call limit in the automatic prompt path |
| `[transport].prompt_budget_ms` | `10000` ms | `ALEXANDRIA_PROMPT_BUDGET_MS` | Shared deadline for project lookup, handshake, recall, reminders, and reconnect attempts |

Timeouts must be positive finite numbers; per-call timeout must be less than or equal to the whole-path budget, and the whole-path budget must not exceed `120000` ms. Invalid values fail configuration rather than clamp. Each stage shares the same whole-path deadline; the configured call timeout is not a fresh allowance per stage. These settings govern automatic prompt work, not explicit native MCP calls.

The old automatic-storage settings are obsolete: `ALEXANDRIA_AUTO_STORE`, `ALEXANDRIA_EXTRACT_MODEL`, `ALEXANDRIA_EXTRACT_TIMEOUT_MS`, and `[store]` extraction fields (`enabled`, `extract_model`, `extract_timeout_ms`) are ignored with a warning. Remove them and explicitly call `store_memory` or `import_document` when something should be saved.

## Reminders

The server does not run a reminder timer. This extension asks once per prompt, so no client means no automatic delivery. The project selector it sends is the issued ID produced by the project-resolution flow above, not a repository name or basename. The server validates ownership and targeting; a failed check is not retried as a global check. See the [memory skill](../../skills/alexandria-memory/SKILL.md) for reminder semantics and safe tool use.

## Failure behavior and limits

Recall and reminder handling fail open with a warning where possible: a server outage does not block the agent turn. Prompt work is cancellable and bounded by the shared deadline; cancellation is distinguished from transport failure, and connections are dropped only for transport failures. The extension prewarms its MCP connection at load to keep the normal initial handshake off the first prompt path. Diagnostics are appended to `$XDG_STATE_HOME/alexandria-companion/prompt-path.jsonl` (default `~/.local/state/alexandria-companion/`), asynchronously and without blocking the prompt path.

The extension currently uses `retrieve_memories` rather than the two-phase `recall` tool. Sessions are grouped through session IDs but not finalized. Exact in-process repeat suppression is not a substitute for semantic deduplication.

See [`contrib/pi/README.md`](../../README.md) for the skill/extension comparison and installation options, and [docs/configuration.md](../../../../docs/configuration.md) for the wider configuration reference.
