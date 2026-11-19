# Alexandria client-side integrations for pi

Optional client-side pieces for using Alexandria from [pi](https://github.com/earendil-works/pi-mono). They are not part of the MCP server and do not install automatically. The skill is guidance; the companion extension adds prompt-time recall and reminder delivery. Neither installed path independently writes memories from observed conversation content.

## Choose an integration

| | Skill | Companion extension |
| --- | --- | --- |
| What it does | Guides the agent on when to call Alexandria tools | Automatically recalls relevant memories and checks due reminders before a prompt |
| Writes | Agent calls `store_memory` / `import_document` explicitly when appropriate | No automatic observation/extraction writes; explicit agent tool calls remain available |
| Install | Copy a skill directory | Copy extension directory and install its locked runtime dependencies |

## Memory skill

The skill gives explicit trigger guidance for reading, updating, importing, and storing durable information. It uses pi's native MCP tool names, `mcp__alexandria__<tool>`.

```bash
cp -r contrib/pi/skills/alexandria-memory ~/.pi/agent/skills/
```

For project-scoped work, resolve the project first with `mcp__alexandria__resolve_project` and use the returned issued project ID. When no project context is established, use Personal scope; do not guess a project from a directory or basename. Automatic capture was removed: if you previously relied on detectors or session-end extraction, call `store_memory` explicitly for each durable item (or `import_document` for a document) using the skill's guidance.

## Companion extension

The extension does two prompt-time tasks: it retrieves relevant memories and injects them before the agent starts, and it calls `check_reminders` once per prompt. It does not detect corrections/preferences/errors to store and does not run session-end LLM extraction. If you relied on those automatic writers, use the authenticated MCP tools explicitly as directed by the skill.

Install:

```bash
cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria
npm install --omit=dev --ignore-scripts
```

The extension authenticates independently of pi's model-provider credentials. Set `ALEXANDRIA_API_TOKEN` to an Alexandria agent token; it is not read from or sent through provider configuration. The extension also uses `ALEXANDRIA_PROJECT_ID` as an explicit project override. Older/incompatible servers that lack bearer authentication or `resolve_project` are rejected with upgrade guidance; it does not retry without authentication or fall back to an unscoped request.

### Project context

On each prompt, the companion resolves the session directory's `origin` remote (unless `ALEXANDRIA_PROJECT_ID` is set) through `resolve_project`. The client reduces supported HTTPS/SSH remote forms to a descriptor containing host, optional non-default port, and repository path; it removes `.git`, normalizes the host, and discards userinfo, query, and fragment before sending. Git probes run with inherited `GIT_*` overrides removed. The server returns the issued project ID, which scopes recall and reminder checks and is included in the agent-visible prompt context. The resolution cache is keyed by server/account/directory/descriptor and is invalidated when server or account identity changes.

A configured `ALEXANDRIA_PROJECT_ID` is passed to `resolve_project` and must identify an owned project. If no override and no usable origin remote are available, the companion sends no project selector and recall remains Personal-only; it does not infer a project from the checkout basename. Resolution/auth failures fail closed rather than silently widening scope. The Claude hook does not duplicate remote normalization: it supports the explicit project ID override and otherwise stays Personal-only.

### Configuration

The companion reads `$XDG_CONFIG_HOME/alexandria/client.toml` (on macOS, `~/Library/Application Support/alexandria/client.toml` unless `XDG_CONFIG_HOME` is set). `ALEXANDRIA_CLIENT_CONFIG` selects an alternate file. Precedence is defaults → file → individual environment variables. Environment variables override file values.

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
| `[recall].enabled` | `true` | `ALEXANDRIA_AUTO_RECALL=off` | Disable automatic prompt recall |
| `[recall].limit` | `10` | `ALEXANDRIA_AUTO_RECALL_LIMIT` | Retrieval limit |
| `[recall].min_similarity` | `0.45` | `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | Client-side injection threshold; measured with limit 10 |
| `[reminders].enabled` | `true` | `ALEXANDRIA_REMINDERS=off` | Disable per-prompt reminder checks |
| `[transport].prompt_call_timeout_ms` | `5000` ms | `ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS` | Maximum duration of an individual prompt-path MCP call |
| `[transport].prompt_budget_ms` | `10000` ms | `ALEXANDRIA_PROMPT_BUDGET_MS` | One deadline shared by project resolution, handshake, recall, reminders, and reconnect attempts |

Timeout values must be positive finite numbers; `prompt_call_timeout_ms` must be no greater than `prompt_budget_ms`, and `prompt_budget_ms` must be at most `120000` ms. Invalid values fail configuration rather than being clamped. The whole-path budget is one deadline, not a fresh budget per stage. It applies to automatic prompt work, not explicit native MCP calls. The defaults remain 5 s per call and 10 s for the prompt path.

Older extraction settings are ignored with a warning: `ALEXANDRIA_AUTO_STORE`, `ALEXANDRIA_EXTRACT_MODEL`, `ALEXANDRIA_EXTRACT_TIMEOUT_MS`, and the old `[store]` extraction fields (`enabled`, `extract_model`, `extract_timeout_ms`). Remove them; saving memory now requires an explicit `store_memory`/`import_document` call.

See [docs/configuration.md](../../docs/configuration.md) for server and client configuration context, and the companion [README](extensions/alexandria/README.md) for operational details.
