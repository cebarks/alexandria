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

The full guide — how this compares to the `alexandria-memory` skill, when you want it at all, and
how to install it — is in [`contrib/pi/README.md`](../../README.md). This file is the
in-directory reference for behavior and configuration.

## What it does

| Layer | Hook | Behavior | Disable with |
| --- | --- | --- | --- |
| Auto-recall | `before_agent_start` | Sends the user's prompt to `retrieve_memories` — the *server* embeds it; the extension carries no embedding dependency — and injects hits with `similarity >= recall.min_similarity` (inclusive) as an `alexandria` context message. The injected block tells the agent to verify relevance and to `update_memory` anything stale. | `ALEXANDRIA_AUTO_RECALL=off` or `[recall] enabled = false` |
| Reminder delivery | `before_agent_start` | Calls `check_reminders` once per prompt, injects whatever is due into the same message and shows a count notification. | `ALEXANDRIA_REMINDERS=off` or `[reminders] enabled = false` |
| Correction detector | `before_agent_start` | Regex over the prompt for correction-shaped language ("no, use X"). Fires only on unambiguous matches; ambiguous cases are left to the extraction pass. | `ALEXANDRIA_AUTO_STORE=off` or `[store] enabled = false` |
| Preference detector | `before_agent_start` | Regex for forward-looking preference/convention statements ("always do X", "never do Y", "don't do Z"). The stored text starts at the trigger word, so a prohibition keeps its negation; a match with a negation earlier in its clause is not stored at all. | as above |
| Error tracker | `tool_execution_end` → `agent_end` | Pairs a failing tool call with a later success of the same tool and stores the resolution. Errors must contain a recognized signal to be tracked. | as above |
| Dedup tracker | `tool_result` | Records content from agent-initiated `store_memory` / `update_memory` calls (matched by tool-name suffix, so the MCP prefix doesn't matter) so extraction doesn't re-report them | as above |
| LLM extraction | `session_shutdown` | Serializes the conversation, sends it to `store.extract_model`, stores what's left tagged `extracted`. Skipped when the shutdown reason is `reload`. | `ALEXANDRIA_AUTO_STORE=off` or `[store] enabled = false` |

Every store carries pi's session id, `agent_id = "pi"` and the active model id, so the writes are
grouped in Alexandria (`get_session`, `list_sessions(agent_id="pi")`). Correction and preference
stores are tagged `auto-detected` + `source:regex`, error resolutions `auto-detected`, extraction
output `extracted`, so quality can be cut by source later. A store the server rejects shows as a
warning.

The extension itself only ever calls three of the server's 13 tools: `retrieve_memories`,
`check_reminders` and `store_memory`. The rest are the agent's to call directly — `recall`,
`update_memory`, `import_document`, `delete_memory`, `get_session`, `list_sessions`,
`finalize_session`, and the four reminder-management tools. `update_memory` and `delete_memory` are
named in the injected recall block precisely so the agent knows to reach for them.

Heuristic stores are fire-and-forget and never block a turn. Only the extraction pass can add
latency, and only at session end.

Auto-recall and reminder delivery run concurrently in the single `before_agent_start` handler and
merge into one injected message (`customType: "alexandria"`). Each fails on its own: a reminder check
that throws still lets recall inject, and vice versa. The store detectors are separate handlers and
are unaffected by the recall/reminders toggles.

**Install:**

```bash
cp -r contrib/pi/extensions/alexandria ~/.pi/agent/extensions/
cd ~/.pi/agent/extensions/alexandria
npm install --omit=dev --ignore-scripts
```

## Reminders

The server runs no timer. A reminder becomes visible only when a client calls `check_reminders`, so
this extension attempts delivery once per prompt, before the agent starts. The authenticated server
requires an owned typed project handle when a check supplies context; the unchanged companion's
basename default is incompatible with that requirement (see project targeting below).

- **Delivered once.** `check_reminders` consumes what it returns, so a reminder appears exactly once
  — the agent has to act on it or tell the user, because nothing will raise it again. Recurring
  schedules advance to their next fire rather than being consumed (one that has no future fire left
  is consumed on its last delivery).
- **Missed fires coalesce.** If a daily reminder was due three times while nothing checked, the next
  check delivers it once and labels it `(missed 2 earlier occurrence(s))` — the count is the fires
  *after* the one being delivered, not all of them — instead of sending three
  copies. The engine stops counting at 10 000 occurrences, and when it has, the label reads
  `(missed 10000+ earlier occurrence(s), count capped)` rather than presenting a floor as a fact.
- **Recurring means wall clock.** Schedules run in the server's `[reminders].timezone`, so a daily
  09:00 stays 09:00 local across a DST change: a time the spring-forward gap removed does not fire that
  day, and a time a fall-back fold repeats fires once, on its first pass. Delivery rows carry
  `due_at_local` and `timezone` beside the UTC instant, and the injected block shows each row's `id` so
  the agent can `cancel_reminder` without a round trip. More than 10 rows in one check are summarized
  with a pointer to `list_reminders` — they were still consumed, which is why the pointer is there.
- **Project targeting and current compatibility gap.** The authenticated server accepts only an
  existing owned, issued `project:<opaque-id>` for `set_reminder.target_project`,
  `check_reminders.project`, and `list_reminders.target_project`. Copy the actual handle returned by
  `resolve_project` or `list_projects`, not its display name, alias, repository name or basename.
  Resolve/select an owned handle first if none is known; do not invent a result or silently widen a
  project-specific request. Unknown/foreign handles are refused even when no reminders are due.
  This extension does **not** perform that lookup: each check uses the explicit env/TOML override if
  set, otherwise the basename of `git rev-parse --show-toplevel` in the session's directory
  (`ctx.cwd`), otherwise no context. Probes are cached per directory. The unchanged basename default
  is unsupported by the typed server and produces a refused check, not late/global delivery.
- **Escalation stays within the owner account.** On a check with an accepted or omitted context, a
  valid typed target at least the server's `[reminders] escalation_hours` (default 48) overdue can
  deliver in another project context of that same account, labeled
  `[OVERDUE — escalated from project targeting]`. It never delivers another account's reminder,
  regardless of matching basenames. Global reminders are account-local too. Held or unresolved
  historical text targets stay pending and suppressed, never global/escalated; no automatic mapping
  is implemented.
- **Two audiences, unequally.** Every delivery is injected into context (the agent-visible half) and
  *announced* as `⏰ N Alexandria reminder(s) due`. The announcement is the weaker half:
  `ctx.ui.notify` is a no-op wherever pi has no dialog UI — `pi -p`, print mode, a headless run — so
  what it guarantees is that the human's own agent saw it, not that the human did. The rows are
  consumed either way.

An explicit **existing owned project ID** may be supplied through `ALEXANDRIA_REMINDERS_PROJECT`
or `[reminders].project`: the current configuration/getProjectHint/checkReminders path passes that
configured string into `check_reminders.project` without resolving it. Copy an actual successfully
returned handle exactly; an override does not grant ownership, and the server still validates it.
Setting another name/basename does not repair compatibility. Omitting the override inside a repository
still selects the unsupported basename, not a context-free check; when no context is sent, only your
global and eligible escalated reminders can deliver. Automatic issued-handle plumbing is separate
work, not implemented here.

Everything here is fail-open: an unreachable server, a missing `git`, or a malformed response
produces a warning at most, never a blocked turn — and a notification that itself throws (pi's `ctx.ui`
goes stale across a session switch) cannot discard an injection, because the message is decided before
anything touches the UI. A reminder check that gives up at the client may already have consumed its
rows server-side, so that one fire is lost; a malformed or error-shaped response consumes nothing, is
reported, and is retried on the next prompt.

The budget is **10 s for the whole prompt path**, not 5 s per call. Each call is bounded at 5 s, but
they stack — a 2 s git probe, a 5 s handshake, a 5 s call, and one reconnect retry — and pi waits for
this handler before the turn starts, so the number a user feels is the sum. `PROMPT_BUDGET_MS` aborts
the lot and closes the dropped connection (a nulled-but-never-closed client leaked one session per
timed-out prompt). Repeated failures warn once per prompt; that is deliberate, since a permanently
broken delivery path should stay visible rather than latch itself quiet.

The handshake is normally **not** on that path: `prewarm()` connects at extension load, so the first
prompt of a session reuses it. That is a correctness measure rather than an optimisation. The SDK
implements its deadline as a `setTimeout` on pi's main loop, which every other in-process extension
shares, so a stall longer than the remaining budget makes the overdue timer win the race against a
handshake that was succeeding — reporting a healthy server as timed out. Measured: freezing the loop
for 6 s 10 ms into a cold first call produced a false `REQUEST_TIMEOUT` in **6 of 6** fresh processes;
the same freeze against an already-connected client resolved normally in **15 of 15**. Pre-warming
closed it (3 of 3 honoured). It does not cover a reconnect after `resetClient()` — a server restart or
transport failure still handshakes on the prompt path — but cancelling a prompt no longer causes one.

A failure is attributed to whatever actually failed, because the SDK reports an *aborted* request as
`REQUEST_TIMEOUT` and the error text alone cannot distinguish the cases:

| Kind | Meaning | Warns | Drops the connection |
|---|---|---|---|
| `cancelled` | you pressed Esc, or a newer prompt superseded this one | no | no |
| `stalled` | the 10 s path budget expired; the 5 s per-call deadline did not, so the timers themselves ran late | yes | no |
| `transport` | the server or the network genuinely failed | yes | yes |

Cause text comes from the deepest `err.cause`, so a closed port, a dead DNS name and an unroutable
host no longer all read as undici's `fetch failed`:

```
closed port  -> connect ECONNREFUSED 127.0.0.1:3000 [ECONNREFUSED]
dead DNS     -> getaddrinfo ENOTFOUND relativity.grv.st [ENOTFOUND]
no route     -> Request timed out [REQUEST_TIMEOUT]
```

### Diagnostics

Every prompt-path run appends one JSON line to
`$XDG_STATE_HOME/alexandria-companion/prompt-path.jsonl` (`~/.local/state/...` by default), rolling at
256 KB:

```json
{"t":"2026-09-22T20:14:03.112Z","ms":47,"cold":false,"driftMs":0,"recall":"injected","reminders":"none (0 due)"}
```

`cold` says whether that run paid a handshake — the only window measured to lose the race above.
`driftMs` is the worst event-loop lateness observed during the run, which is what separates "the server
was slow" from "the server was fine and something else in pi's process held the loop": a `transport:
Request timed out` row with `driftMs` in the thousands is a client-side stall, not a server problem.
This log exists because the failure is intermittent and had never been observed in the field; read it
with `tail -f` when recall misbehaves. Writing it is async, fire-and-forget and self-swallowing, and
the probe timer is `unref`'d — a diagnostic that could block the loop would be worse than none,
since a blocked loop is the suspected cause.

Stale Streamable HTTP sessions — usually an Alexandria restart — are detected and retried once on a
fresh connection, in every feature.

Extraction routes through pi's `ctx.modelRegistry`, so provider auth (Vertex OAuth, Anthropic keys)
is handled by pi rather than by this extension. If the configured model or provider is unavailable,
it falls back to the session's own model.

## Configuration

Config file: `$XDG_CONFIG_HOME/alexandria/client.toml` (`~/Library/Application Support/alexandria/client.toml`
on macOS), overridable with `ALEXANDRIA_CLIENT_CONFIG`. Environment variables win over the file,
which wins over defaults. Unparseable TOML warns and falls back to defaults rather than failing.

`enabled = false` in the TOML file disables a feature the same way `off` does in the environment,
except an explicit env var always overrides the file.

See [docs/configuration.md](../../../../docs/configuration.md) for the full reference.

### Example `client.toml`

```toml
[server]
url = "http://127.0.0.1:3000/mcp"

[recall]
enabled = true
limit = 10
min_similarity = 0.45   # only valid at limit = 10; see docs/configuration.md [recall]

[store]
enabled = true
extract_model = "vertex/claude-haiku-4-5"
extract_timeout_ms = 5000

[reminders]
enabled = true
# Configure project only with an actual owned ID returned by resolve_project/list_projects.
# No automatic lookup: the unchanged repository-basename default is unsupported.
# project = "project:<actual-issued-id>"
```

### Environment Variables

| Variable | Default | Description |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria server MCP endpoint |
| `ALEXANDRIA_AUTO_RECALL` | (enabled) | Set to `off` to disable auto-recall |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `10` | Max memories to retrieve. A recall lever, not just a cap; paired with `MIN_SIMILARITY` |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.45` | Minimum cosine similarity, inclusive (model-dependent; sits above the server-side `[retrieve] min_similarity` floor). A judgement call for `all-MiniLM-L6-v2`, only valid at `LIMIT=10`: see `[recall]` in [`docs/configuration.md`](../../../../docs/configuration.md) |
| `ALEXANDRIA_AUTO_STORE` | (enabled) | Set to `off` to disable all store behavior — detectors and extraction alike |
| `ALEXANDRIA_EXTRACT_MODEL` | `vertex/claude-haiku-4-5` | Model for the LLM extraction pass |
| `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | `5000` | Extraction timeout in milliseconds |
| `ALEXANDRIA_REMINDERS` | (enabled) | Set to `off` to disable per-prompt reminder checks |
| `ALEXANDRIA_REMINDERS_PROJECT` | (git repo dir name; unsupported by typed server) | Explicit existing owned project ID passed to `check_reminders.project`; copy a real issued handle from `resolve_project`/`list_projects`. No automatic lookup or ownership grant. |
| `ALEXANDRIA_CLIENT_CONFIG` | (XDG default) | Path to an alternate client TOML config |

`ALEXANDRIA_AUTO_RECALL` is the original name of the recall toggle and is still the spelling it
uses; `ALEXANDRIA_REMINDERS` follows the same shape for the reminders feature.

## Development

The type-only Pi SDK dependency is exact-pinned at **1.1.0**. Changing it requires
`npm ci --ignore-scripts`, the typecheck and tests (`just ext-test`), and a real
SDK/host behavior check: type compatibility alone cannot prove event or injection
behavior. Production installs omit this development dependency; CI does not load
an actual Pi host. Keep lifecycle scripts disabled for every install path.

## Known gaps

- No unit tests for the extraction prompt or its truncation (the detectors, the transcript
  serializer and the response parser do have them, in `tests/`).
- Sessions are grouped but never finalized: nothing calls `finalize_session` at shutdown yet.
- Regex stores are not deduplicated across rewordings. The session buffer catches a repeat of the
  same text in one process, nothing catches "use jj" today and "prefer jj" next week. Deferred to
  [#27](https://github.com/cebarks/alexandria/issues/27).
- Uses `retrieve_memories` only; the two-phase `recall` tool is never called.
- Nothing fires a reminder on its own: with reminders off, delivery depends on the agent happening
  to call `check_reminders`.

See [`contrib/pi/README.md`](../../README.md) for the full limitation list.
