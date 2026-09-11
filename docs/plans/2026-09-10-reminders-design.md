# Alexandria Reminders Design

**Date**: 2026-09-10
**Status**: Draft — reviewed, ready for implementation planning

## Overview

Add a reminder system to Alexandria: agents can schedule messages ("remind me / remind
yourself about X") that are delivered on the **next interaction** after they come due, to
**both** channels — injected into agent context and surfaced as a human-visible pi
notification.

Core design decisions (from brainstorming, 2026-09-10):

| Decision | Choice | Rationale |
| --- | --- | --- |
| Audience | Both agent + human | Reminders should reach whoever can act on them |
| Due-time behavior | Lazy — wait for next interaction | No watcher daemon, no OS coupling, transport-agnostic (stdio + HTTP) |
| Recurrence | Native recurring (one-shot + repeating) | "remind me every Friday" must survive without agent cooperation |
| Recurrence language | Named patterns (default) + cron escape hatch | Structured fields are self-validating at set-time; cron covers oddballs |
| Scoping | Per-reminder target (global \| project) + provenance metadata | Balances noise vs. silent-loss |
| Silent-loss prevention | Overdue project reminders escalate to global after `escalation_hours` | Backstop for projects that are never opened again |
| pi delivery | Generalized companion extension with feature toggles | One extension, configurable features — not a pile of single-purpose extensions |

**Explicitly out of scope (YAGNI)**: snooze (cancel + re-set covers it), OS-level
notifications at exact due time, mid-session interrupts/agent wake-up, promotion of
delivered reminders into memories.

## Data Model

One new table, `reminder`, added via the next numbered forward-only migration (`v00X`,
tracked in `system_config` per existing convention). Reminders live **outside** the
fact/embedding graph — no embeddings, no clustering, no `derived_from` lineage. They are
scheduled messages, not memories.

```
reminder {
  id,
  message: string,                  // standalone-readable delivery text
  target: global | project:<name>,  // delivery targeting
  provenance: {                     // set-time metadata, display only
    project?: string,
    session_id?: string,
    note?: string
  },
  schedule: one_of {
    once:    { due_at: datetime },  // UTC
    pattern: { freq: daily | weekly | monthly,
               time: "HH:MM",
               weekdays?: [...],    // for weekly (and optional multi-weekday daily)
               day_of_month?: int },// for monthly; skips short months (cron-style)
    cron:    { expr: string }       // escape hatch, parsed + validated at set-time
  },
  next_due_at: datetime,            // UTC, advanced statelessly at delivery
  status: pending | cancelled,      // soft-cancel; no separate 'delivered' state
  created_at, cancelled_at?, last_delivered_at?, delivered_count
}
```

**No background timer anywhere.** Due-ness is pure query-time evaluation:
`status = pending AND next_due_at <= now()`. When a recurring reminder is consumed at
delivery, the server advances `next_due_at` to the *first occurrence strictly after now*,
skipping all missed occurrences and reporting `missed_occurrences: N` in the delivery
payload. Server downtime, lazy delivery, and coalescing all fall out of this one
stateless rule — no `tokio::spawn`, identical behavior in stdio and HTTP mode.

## MCP Tools

Four new tools in the `#[tool_router]` block of `AlexandriaServer`, descriptions in the
existing directive style. The separate `#[tool_handler]` block is untouched.

### `alexandria_set_reminder`

Params: `message`, `schedule` (exactly one of once/pattern/cron), optional `target`
project name (default global), optional provenance note.

All schedule validation happens **here**, loudly:

- cron strings are parsed at set-time; invalid → error with hint, never stored
- pattern fields checked (weekday names, `day_of_month` 1–31; day-31 monthly skips
  short months)
- response echoes `next_due_at` **plus the next 3 fire times**, so the agent can confirm
  "Fridays 9am, right?" back to the user before anything is stored wrong

Time input: ISO-8601. Explicit offsets honored. **Naive timestamps are interpreted in the
configured timezone** (`[reminders].timezone`) — documented; no ambiguity for agents that
pass local wall-clock strings.

### `alexandria_check_reminders`

The delivery tool. Params: `context: { project?: string }`.

Returns due reminders matching the targeting rule:

- `global` target → always matches
- `project:<name>` target → matches when `name == context.project`
- anything overdue longer than `escalation_hours` → matches regardless of target
  (labeled "overdue, originally for project X")

**This call consumes**: stamps `last_delivered_at`, bumps `delivered_count`, advances
recurring `next_due_at` past now, and reports `missed_occurrences` for coalesced
recurrings. Best-effort-once semantics — a client crash between fetch and display loses
that occurrence; acceptable, documented in the tool description.

### `alexandria_list_reminders`

Params: status filter (`pending` | `cancelled` | all, default pending), optional project
filter. Returns human-readable schedule renderings ("every Friday at 09:00") alongside
raw fields.

### `alexandria_cancel_reminder`

Params: reminder id. Soft-cancel (`status = cancelled`, `cancelled_at` stamped), matching
the project's snapshot-rather-than-destroy posture.

### Recall piggyback

`retrieve_memories` and `recall` responses gain a `due_reminders` array — a cheap
query-time check, **read-only, never consumes**. Clients without any pi extension still
see overdue items every time they touch memory; items keep nagging on every recall until a
real `check_reminders` consumes them. Works over both transports, no client cooperation
required.

## Delivery: Generalized pi Extension

The existing `alexandria-auto-recall` extension (which already grew recall + heuristic
detectors + LLM extraction in the auto-store design) is generalized into a single
companion extension built from independent, individually-toggleable feature modules:

```
contrib/pi/extensions/alexandria/
├── src/
│   ├── index.ts              # loads config, registers hooks, dispatches to features
│   ├── config.ts             # client.toml + env precedence, per-feature sections
│   ├── mcp-client.ts         # shared Alexandria MCP client (existing)
│   └── features/
│       ├── types.ts          # Feature interface
│       ├── recall.ts         # existing auto-recall behavior, moved
│       ├── store.ts          # existing detectors + extraction wiring, moved
│       └── reminders.ts      # new: due-reminder check + injection
├── package.json / tsconfig.json
└── tests/
```

Each feature implements one interface (`onPrompt(ctx) → { inject?, notify? }` plus
lifecycle hooks where a feature needs them, e.g. `store`'s `session_shutdown`
extraction). `index.ts` runs enabled features, aggregates their context injections into
marked blocks, and surfaces notifications.

**Failure isolation is per-feature**: recall erroring never suppresses reminders and vice
versa. Every path fails open — warn via `ctx.ui.notify` at most, never block the turn
(matches the existing extension's posture).

### Reminders feature behavior

Hook: `before_agent_start`, on every prompt:

1. Compute project hint: `git rev-parse --show-toplevel` from cwd → basename; fails open
   to no hint if not in a repo
2. Call `alexandria_check_reminders` with `{ project: <hint> }` (HTTP, same client as recall)
3. On due items:
   - **Agent channel**: inject a clearly-marked `⏰ Due reminders:` block — message, target,
     provenance, "missed N occurrences" for coalesced recurrings — formatted for
     visibility, never buried among memory hits
   - **Human channel**: `ctx.ui.notify("⏰ N reminders due", "info")`
4. Fail-open on every error path (unreachable server, bad response, git failure)

**Worktree handling**: `show-toplevel` from a worktree returns the worktree dir, so the
git-root basename hint can miss (`alexandria-wt-fix` ≠ `alexandria`). Accepted: the hint
only decides *early* vs *escalated* delivery, never lost vs found — the server's
escalation backstop guarantees delivery within `escalation_hours`. Optional
`ALEXANDRIA_REMINDERS_PROJECT` env override (direnv-friendly) for exactness.

**Cadence**: check runs per-prompt, not on a timer — consistent with lazy delivery. A
long idle session with pi open won't interrupt; the next prompt picks everything up.

Two HTTP round trips per prompt when recall + reminders are both on. Local server,
sub-ms each; batching into one server call isn't worth the coupling.

## Configuration

### Server — new `[reminders]` section

Follows the existing precedence chain (defaults → `config.toml` → `ALEXANDRIA_*` env):

```toml
[reminders]
timezone = "Europe/Stockholm"   # IANA name; default: system-local.
                                # Used for naive datetime input + pattern/cron evaluation
escalation_hours = 48           # project-targeted reminders go global after this overdue
```

New workspace dependencies: `cron` (parse + next-fire computation) and `chrono-tz`
(IANA tzdb). Both build on the existing `chrono`.

### Client — feature toggles

Same precedence chain (defaults → `client.toml` → env vars):

```toml
[features.recall]
enabled = true                  # env: ALEXANDRIA_RECALL=off

[features.store]
enabled = true                  # env: ALEXANDRIA_AUTO_STORE=off (existing)

[features.reminders]
enabled = true                  # env: ALEXANDRIA_REMINDERS=off
```

Legacy `ALEXANDRIA_AUTO_RECALL=off` keeps working as an alias for
`features.recall.enabled = false` so existing installs don't silently turn recall back on.
Migration is a directory rename + README note (`cp -r` install, no package manager).

## Error Handling

- All schedule validation at set-time — bad cron/patterns never reach storage; errors
  include the computed next-fire preview so agents can self-correct
- `check_reminders` consumption is best-effort-once, documented in the tool description
- Migration is forward-only, numbered, tracked in `system_config`
- SurrealDB 3.2 gotchas respected: no `SELECT value` (use `SELECT * FROM reminder`),
  `DELETE reminder WHERE ...` (no FROM), pre-parsed `RecordId` via `.bind()`,
  `record_id_to_string()` for output formatting, `#[derive(SurrealValue)]` on query
  result structs

## Testing

**Unit (engine-level, pure — no DB)**:

- Next-fire computation: daily / weekly / monthly patterns, cron expressions
- **DST transitions**: daily 09:00 across spring-forward and fall-back
- Day-31 monthly skipping short months
- Coalescing: simulated downtime → correct `missed_occurrences` count and
  `next_due_at` strictly after now
- Naive datetime interpretation in configured tz; explicit offsets honored

**Integration (`connect_embedded()`, in-memory)**:

- Full lifecycle: set → check → consume → advance → check again (no re-delivery)
- Escalation: project-mismatch delivery before/after `escalation_hours`
- Cancel hides from check; cancelled items invisible to piggyback
- Recall piggyback is read-only: `next_due_at`/`delivered_count` unchanged after
  `retrieve_memories`
- Duplicate-check behavior after consumption

**Extension**:

- Config alias resolution (`ALEXANDRIA_AUTO_RECALL=off` → recall disabled)
- Per-feature failure isolation (one feature throws → others still deliver)
- Fail-open on unreachable server (turn proceeds, notify at most)

## Open Questions

1. **Generalized extension name**: `extensions/alexandria/` proposed. If a name collision
   or discoverability concern surfaces during implementation, `alexandria-companion` is
   the fallback. Low stakes — it's a `cp -r` install.
2. **`store` feature refactor depth**: the auto-store design's detectors/extraction code
   is already structured into modules. Wrapping them in the Feature interface may be
   mechanical or may need light surgery; implementation plan should scope this so the
   reminders feature doesn't block on a full store-side rewrite.
