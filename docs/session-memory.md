# Session Memory

Memories are grouped into **sessions** — an optional, client-named bucket that lets an agent
answer "what did we learn in that conversation?" and "what do I know from *just* this
conversation?" without re-deriving either from tags.

Sessions are opt-in per call. Nothing in Alexandria requires them, and memories stored without
one behave exactly as they always have.

## Data model

Schema `v005_session.surql`:

```text
session
├── external_id    string       -- client-supplied handle; UNIQUE index
├── agent_id       option<string>
├── model          option<string>
├── started_at     datetime     -- default time::now()
├── ended_at       option<datetime>
├── summary        option<string>
├── memory_count   int          -- default 0
└── tags           array<string>

(session)->contains_session_memory->(fact)
```

A session is linked to its memories by `contains_session_memory` graph edges, not by a column on
`fact`. The same memory can therefore belong to several sessions; membership is additive and
never moves or copies the fact. `memory_count` is a denormalized counter maintained on write, not
a computed `count()`.

`external_id` is the only identity that matters to clients. It is an opaque string the caller
chooses (pi's session UUID, a ticket number, `"2026-08-27-auth-refactor"` — anything), and it is
what you pass to every session tool below. SurrealDB's own record ID for the session is internal.

## Lifecycle

```text
store_memory(session_id="s1")   # implicit create of `s1`, edge to the new fact, count++
store_memory(session_id="s1")   # edge + count++
retrieve_memories(session_id="s1")   # search scoped to s1's facts
get_session(session_id="s1")         # metadata + every fact, oldest first
finalize_session(session_id="s1", summary=..., tags=[...])   # close it out
```

**Creation is implicit.** Passing an unknown `session_id` to `store_memory` creates the session on
first use — there is no `create_session` tool, and no need to check for existence first.

**`ended_at` means "last activity," not "closed."** Every store bumps `memory_count` and refreshes
`ended_at`. That field is only *also* the close timestamp when `finalize_session` writes it, so
`ended_at` alone cannot tell you whether a session was finalized. Check `summary`: an unfinalized
session has `summary: null`.

## Tools

| Tool | Session behavior |
| --- | --- |
| `store_memory` | Optional `session_id`. Auto-creates the session, relates the new fact, bumps the counter. |
| `retrieve_memories` | Optional `session_id` scopes the candidate set to that session's facts before ranking. |
| `get_session` | Takes `session_id`; returns session metadata plus every linked memory (id, content, tags, confidence, `created_at`), ordered oldest first. Errors if the id is unknown. |
| `finalize_session` | Takes `session_id` and optional `summary` / `tags`; sets `ended_at = time::now()` plus whichever fields were supplied. Errors if the id is unknown. |

Both `Option` fields on `finalize_session` are genuinely optional: calling it with only
`session_id` closes the session without recording a summary.

## Current limitations

These are real gaps in the shipped implementation, not usage advice:

- **No session enumeration.** There is no `list_sessions` tool, and sessions are not reachable
  through `recall` (which walks clusters, not sessions). `get_session` requires knowing the
  `external_id` already, so a session you failed to record the id for is not recoverable through
  MCP — only through the debug UI or a direct query.
- **`agent_id` and `model` are dead columns today.** The `session` table defines both and
  `SessionRepo::create` accepts them, but the MCP path calls it with `(None, None)` and no tool
  parameter exposes either. Nothing populates them.
- **Session-scoped search does not filter soft-deleted memories.** The unscoped path queries
  `fact WHERE deleted = false`; the session path walks edges through `SessionRepo::get_memories()`,
  which fetches each fact with `get_fact()` and never checks `deleted`. So
  `retrieve_memories(session_id: ...)` can return a memory the user asked you to forget, and
  `get_session` lists deleted memories with no marker distinguishing them. Tracked as a
  follow-up — until it is fixed, treat session-scoped hits as needing a sanity check.
- **The pi extension does not populate sessions.** `contrib/pi/` stores and retrieves memories
  without a `session_id`, so auto-store/auto-recall traffic is ungrouped. Session tools are for
  agents that decide to use them explicitly.

## SurrealDB 3.2 gotchas in this code path

Non-obvious, and easy to reintroduce:

- `session` is a **reserved word** in SurrealDB 3.2 — every query must write `` `session` `` with
  backticks, and the table is `SCHEMAFULL` so an undefined field silently fails.
- `$session` is **also reserved** (it is SurrealDB's own session variable). Bind parameters for a
  session record ID must use another name — `session_repo.rs` uses `$sess`.
- `RELATE` needs a pre-parsed `RecordId` passed through `.bind()`; an inline `type::record()` in a
  `RELATE` statement fails.
- `SELECT * FROM value` is unusable — `value` is reserved as well (see the project-wide list in
  [AGENTS.md](../AGENTS.md)).
