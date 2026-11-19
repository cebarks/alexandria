---
name: alexandria-memory
description: Use the Alexandria agent-memory MCP tools (mcp__alexandria__store_memory, mcp__alexandria__retrieve_memories, mcp__alexandria__recall, mcp__alexandria__update_memory, mcp__alexandria__import_document, mcp__alexandria__delete_memory, mcp__alexandria__list_sessions, mcp__alexandria__get_session, mcp__alexandria__finalize_session) and its reminder tools (mcp__alexandria__set_reminder, mcp__alexandria__check_reminders, mcp__alexandria__list_reminders, mcp__alexandria__cancel_reminder) to persist and recall durable facts, decisions, and preferences across sessions, and to schedule and manage future follow-ups. Use PROACTIVELY at the start of tasks in known projects/domains, whenever the user references past context ("last time", "we decided", "like before") or asks for a future nudge ("remind me", "don't let me forget", "check this tomorrow"), and immediately after learning something worth keeping (a preference, an architectural decision + rationale, a bug's root cause, a correction) — not only when explicitly asked to remember or recall.
---

# Alexandria Memory

Alexandria is a persistent, cross-session agent memory server (semantic search + heat-based
recency + graph clustering + scheduled reminders) reachable via MCP tools when the `alexandria`
server is connected.
Its whole value only materializes if it's actually used — an agent that never calls it behaves
exactly like one with no memory at all. Default to using it; don't wait for an explicit
"remember this" / "check your memory" instruction.

## When to read memory (do this unprompted)

- Starting a task in a project/domain you've plausibly touched before (early in the session,
  before diving into research you might have already done).
- The user references past context: "last time", "we decided", "like before", "you said",
  "remind me what we decided".
- The user asks for a future nudge ("remind me at 3", "don't let me forget to X") — see Reminders.
- Before re-deriving an architectural decision, re-debugging something, or re-asking a
  preference question the user may have already answered in a prior session.
- Before proposing an approach that has tradeoffs — check whether a prior decision/rationale
  already exists so you don't contradict it silently.

Tool choice:

- **`mcp__alexandria__retrieve_memories`** — specific lookup, you know roughly what you're searching for. Pass a
  natural-language statement of the fact/topic (not a question). Returns ranked hits with
  similarity + tags.
- **`mcp__alexandria__recall`** — open-ended/broad exploration ("what do we know about X", "what's the state of
  Y"). Call once with no `scope_handle` to get candidate clusters, then call again with the
  returned `scope_handle` to narrow into the most relevant one.

## When to write memory (do this unprompted)

Store as soon as something durable and non-obvious emerges — don't wait to be told:

- A user preference or convention (tooling choice, style rule, workflow habit).
- An architectural/design decision **and its rationale** (the "why", not just the "what" —
  rationale is what saves a future re-litigation of the same tradeoff).
- The root cause of a bug once found (not the symptom).
- A non-obvious gotcha, footgun, or platform/library quirk you had to discover the hard way.
- A correction the user gives you about something you got wrong.

Tool choice:

- **`mcp__alexandria__store_memory`** — new fact. Write `content` as a standalone statement that still makes
  sense without today's conversation (no "as discussed above", no pronouns without antecedents).
  Add `tags` for the project/domain so future retrieval scopes well.
- **`mcp__alexandria__update_memory`** — you found that an *existing* memory is stale/wrong. Prefer this over
  `mcp__alexandria__store_memory` for corrections — it re-embeds if content changed and preserves the old
  version via a `derived_from` lineage edge instead of leaving a stale duplicate floating
  around.
- **`mcp__alexandria__import_document`** — bulk reference material (a design doc, README, spec, meeting notes)
  the user shares or points at that's worth retaining long-term. Chunks automatically
  (heading/paragraph/fixed-size) and links chunks back to the source document.
- **`mcp__alexandria__delete_memory`** — only when the user explicitly wants something forgotten. This is a
  soft-delete; for corrections, prefer `mcp__alexandria__update_memory` so the lineage survives.

## Sessions

`mcp__alexandria__store_memory` accepts an optional `session_id` — an opaque handle you choose, typically
this conversation's identifier or a task name — which groups that memory under a session and creates
the session on first use. Worth doing when the grouping itself is the useful thing: "everything we
learned about the auth refactor" stays retrievable as a unit even though the memories are
topically scattered across clusters.

- **`mcp__alexandria__list_sessions`** — enumerate sessions newest-first, filterable by `agent_id`, `tag`
  and whether they were finalized, with `limit`/`offset` paging. This is how you find a session whose
  id you did not keep.
- **`mcp__alexandria__get_session`** — review everything stored in one session, oldest first, with its
  metadata. Use when the user asks what was captured in a specific past conversation, or before
  writing a session summary so you don't restate something already stored.
- **`mcp__alexandria__finalize_session`** — set the session's summary, tags, and end timestamp. Call once
  as work wraps up; it is what makes a session skimmable later instead of a pile of fragments.

Rules that matter: a session's `ended_at` is refreshed by every store, so it tracks last activity,
not closure — an unfinalized session still has `ended_at` set, and `summary: null` is the real signal
that it was never closed. Sessions are not searchable by *content*: `mcp__alexandria__list_sessions`
enumerates them by metadata, and `mcp__alexandria__get_session` needs an id you already have or one you just
got from the list. Still worth recording the id you chose somewhere durable if you expect to revisit
it. See `docs/session-memory.md` in the Alexandria repo for details.

## Reminders

Alexandria also schedules standalone messages ("remind me to X") that are delivered into the agent
context on the first interaction after they come due. Under Claude Code nothing reaches the user
automatically — surfacing a delivered reminder is your job. There is no background
timer and no mid-session wake-up: a reminder waits for the next interaction, in every transport.

When to set (do this unprompted, like writes):

- The user explicitly asks to be reminded ("remind me at 3", "don't let me forget to renew the cert").
- A time-bound follow-up this conversation can't finish ("check the deploy at 3pm", "ask me tomorrow
  whether the migration landed").
- Resolve relative phrasing ("in 20 minutes", "tomorrow morning") to an explicit datetime against the
  current time before calling — the server never sees your conversation.

Tool choice:

- **`mcp__alexandria__set_reminder`** — schedule a one-shot or recurring message; exactly one of the three
  kinds below. Write `message` so it still makes sense in a session with none of today's context.
- **`mcp__alexandria__list_reminders`** — what is scheduled and when it fires; filter by `status`
  (`pending` default, `delivered`, `cancelled`, `all`) and `target_project`. This is where you find an
  id to cancel.
- **`mcp__alexandria__cancel_reminder`** — soft-cancel by id when the user says it's no longer needed, or
  after a delivery for a recurring reminder the user now wants stopped.

Schedule kinds — give exactly one:

- one-shot: `due_at: "2026-09-12T09:00:00"` — ISO-8601; an explicit UTC offset is honored, a naive
  time is read in the server's reminders timezone.
- pattern: `pattern: {freq: "weekly", time: "09:00", weekdays: ["fri"]}` — daily/weekly/monthly at a
  wall-clock `HH:MM`; weekly needs `weekdays`, monthly needs `day_of_month` 1-31 (short months skip
  the 31st rather than sliding to the 1st).
- cron: `cron: "0 0 1,15 * *"` — the escape hatch for what patterns can't express (this one:
  midnight on the 1st and 15th). Prefer patterns; use cron only when patterns fall short. Three
  dialect caveats, all silent when you get them wrong: 5, 6 or 7 fields are accepted (a 5-field
  expression gets seconds prepended; `@daily` is rejected, write it out); numeric days of week are
  **1=Sunday through 7=Saturday** — not the usual 0-6 — so `1-5` means Sunday to Thursday and `0` is
  rejected outright, so always write days as names (`MON-FRI`, `SAT`); and day-of-month and
  day-of-week are **ANDed**, not ORed as in Vixie cron, when both are restricted — `0 9 13 * FRI` is
  Friday-the-13th-only, so "the 13th *or* every Friday" is two reminders.

Recurring times are wall clock in that timezone, so across a DST transition a daily 09:00 stays 09:00
local: a time the spring-forward gap removed does not fire that day, and a time a fall-back fold
repeats fires **once**, on its first pass. One wall-clock reading is one occurrence.

Schedules are parsed and validated at set time — a bad cron or impossible time errors back at you, it
is never stored to fail later, and a response may carry a `warning` naming a schedule that is valid but
not what you meant (a restricted day-of-month with a restricted day-of-week, a `due_at` already past). The response echoes the parsed `schedule`, `next_fire_preview`, and the
`timezone` it was evaluated in: **confirm those against what the user asked for before you end the
turn** — a mis-parsed "every Friday" is cheapest to fix while they're still there. Optional `note` and
provenance fields ride along at delivery for context.

Targeting: reminders are **account-bound**. For a project-specific follow-up, use an existing owned,
issued `project:<opaque-id>` returned by `mcp__alexandria__resolve_project` or
`mcp__alexandria__list_projects`. Copy that handle exactly into `set_reminder.target_project`,
`check_reminders.project`, and `list_reminders.target_project`. A display name, alias, repository name,
or checkout basename is not that handle; unknown and foreign handles are refused, even if no rows
are due.

If you only know a basename/name, first propose resolving the project or selecting it from the owned
project listing, using the available tool's schema. Wait for an actual successful result before using
its handle. Do not invent a resolver result, an issued ID, or an account-management RPC. If those
tools are unavailable, explain that an owned handle is needed. Do not silently omit a selector and
widen a project-specific request. For deliberately global reminders or account-wide/context-free
checks, omit the respective optional selector instead.

Global delivery and overdue escalation stay **within the reminder owner's account**. A valid project
target at least `[reminders].escalation_hours` overdue may deliver in another project context of that
same account, flagged `escalated: true`; it never transfers to another account, even with the same
checkout basename. Held rows and unresolved historical text targets stay pending and suppressed,
not global or escalated. There is no automatic historical mapping.

The Pi companion resolves the session directory's sanitized `origin` remote with
`mcp__alexandria__resolve_project`, then uses the returned owned project ID for recall and reminder
checks. Userinfo, query and fragment are discarded before remote identity is sent; absent a usable
remote and absent `ALEXANDRIA_PROJECT_ID`, the companion sends no project selector and remains
Personal-only. The explicit override is resolved and validated too. Authentication uses the
companion's `ALEXANDRIA_API_TOKEN`, independently of model-provider credentials; an incompatible
server requires upgrade, never an anonymous or unscoped fallback. Automatic observation stores and
session-end extraction were removed: when a durable fact should be retained, call
`mcp__alexandria__store_memory` explicitly, or `mcp__alexandria__import_document` for source material.

The Claude recall hook has a narrower context contract: it resolves only an explicit
`ALEXANDRIA_PROJECT_ID` and does not inspect git remotes. Without the override it stays Personal-only.
It authenticates with `ALEXANDRIA_API_TOKEN`; a failed project lookup never becomes unscoped recall.
Claude hooks no longer store prompt-regex observations or extract transcripts. Use the explicit
`store_memory`/`import_document` tools when there is durable information to retain.

Delivery happens only when something calls **`mcp__alexandria__check_reminders`**, and the Claude Code
hooks here do not call it — they only auto-recall. So under Claude Code you are that something: call
it when the user asks "anything due?", when a reminder they expected hasn't shown up, or when you have
just set a reminder that was already due, passing the actual owned issued project handle so typed
targets match (or omitting context for an intentional account-wide check). That call is the *only* thing that consumes reminders (recurring ones advance; missed fires
coalesce rather than trickle). The `due_reminders` array on `mcp__alexandria__retrieve_memories` /
`mcp__alexandria__recall` responses is a read-only safety net (a small oldest-due sample) that is
*untargeted within your account* and consumes nothing: eligible entries carry `global` or a typed
project handle. Another owned project's entry is informational, not consumed; delivery occurs through
a matching or eligible escalated `mcp__alexandria__check_reminders` call. Held/unresolved targets are
excluded from that sample but remain inspectable in your unfiltered reminder history.

## Project scope and explicit writes

Without an explicit project context, ordinary reads are Personal-only; they do not search every project.
For project-specific work, call `mcp__alexandria__resolve_project` with the available project identity,
then use the returned owned ID with the relevant tool's project/context parameter. Do not infer an ID
from a name or checkout basename. Project writes require an explicit project target; a session ID is
not a substitute for a scope. The Pi companion can resolve a sanitized git origin automatically, but
an agent using the tools directly must resolve the project before issuing a project-scoped request.

## Guidelines

- **Store durable information explicitly.** Use `store_memory` for a standalone durable fact,
  decision, preference, correction, or resolved bug; use `import_document` for reference material.
  Client integrations do not automatically extract or write observed conversation content.
- **Write for a stranger.** Assume the reader has zero context from this conversation.
- **Don't narrate it.** Store/retrieve silently as part of normal work, the same way you'd read
  a file — don't announce "let me check my memory" unless the result changes what you tell the
  user.
- **Surface conflicts.** If a retrieved memory contradicts what you're about to do or say, flag
  it to the user rather than silently picking one.
- **Batch queries when exploring multiple angles** — call `mcp__alexandria__retrieve_memories` a few times with
  varied phrasing rather than one query trying to cover everything, same principle as varying
  web-search queries.

## Availability

These tools come from the `alexandria` MCP server (HTTP, `http://127.0.0.1:3000/mcp`). A Claude
Code MCP connection must use an authorized bearer credential when the server requires authentication;
the hook's `ALEXANDRIA_API_TOKEN` configures only the hook, not the native MCP connection. If the
tools aren't present, the server may not be running — check `systemctl --user status alexandria` (if
deployed as a systemd user service) before concluding memory isn't available for this session.
