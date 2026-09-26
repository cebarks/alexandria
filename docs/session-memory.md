# Session Memory

Memories are grouped into **sessions** — an optional, client-named bucket that lets an agent
answer "what did we learn in that conversation?" and "what do I know from *just* this
conversation?" without re-deriving either from tags.

Sessions are opt-in per call. Nothing in Alexandria requires them, and memories stored without
one behave exactly as they always have.

## Data model

Schema `v005_session.surql`, amended by `v006_drop_session_memory_count.surql`:

```text
session
├── external_id    string       -- client-supplied handle; UNIQUE index
├── agent_id       option<string>
├── model          option<string>
├── started_at     datetime     -- default time::now()
├── ended_at       option<datetime>
├── summary        option<string>
└── tags           array<string>

(session)->contains_session_memory->(fact)
```

A session is linked to its memories by `contains_session_memory` graph edges, not by a column on
`fact`. The same memory can therefore belong to several sessions; membership is additive and
never moves or copies the fact.

There is **no stored `memory_count`**. Migration v006 dropped the column: `delete_memory` soft-deletes
a fact and cannot decrement a counter that was set at store time, so any stored tally drifts. Counts
are **derived** from the `contains_session_memory` traversal, with the same `deleted = false` filter
`get_memories()` applies, which is why `SessionRepo::list` can never claim a memory that the session
detail view would hide.

`external_id` is the only identity that matters to clients. It is an opaque string the caller
chooses (pi's session UUID, a ticket number, `"2026-08-27-auth-refactor"` — anything), and it is
what you pass to every session tool below. SurrealDB's own record ID for the session is internal.

## Lifecycle

```text
store_memory(session_id="s1")   # implicit create of `s1`, edge to the new fact
store_memory(session_id="s1")   # edge
retrieve_memories(session_id="s1")   # search scoped to s1's facts
get_session(session_id="s1")         # metadata + every fact, oldest first
list_sessions(agent_id="pi", finalized=false)   # find a session id you don't have
finalize_session(session_id="s1", summary=..., tags=[...])   # close it out
```

**Creation is implicit.** Passing an unknown `session_id` to `store_memory` creates the session on
first use — there is no `create_session` tool, and no need to check for existence first.

**`ended_at` means "last activity," not "closed."** Every store refreshes `ended_at`. That field is
only *also* the close timestamp when `finalize_session` writes it, so `ended_at` alone cannot tell you
whether a session was finalized. Check `summary`: an unfinalized session has `summary: null`.

**Listing order is total.** `SessionRepo::list` orders by `ended_at DESC, external_id ASC`.
A null `ended_at` (a session created but never touched) sorts below every datetime, so never-active
sessions land in the tail — but `external_id` is the load-bearing half: it is UNIQUE-indexed, so the
pair is a total order. Ordering by `ended_at` alone left the ties unspecified, and because the debug
UI pages with `LIMIT`/`START` that is a correctness bug, not a cosmetic one — page 2 could repeat a
session page 1 showed while silently dropping another.

## Tools

| Tool | Session behavior |
| --- | --- |
| `store_memory` | Optional `session_id`. Auto-creates the session and relates the new fact to it. |
| `retrieve_memories` | Optional `session_id` scopes the candidate set to that session's facts before ranking. |
| `get_session` | Takes `session_id`; returns session metadata plus every linked memory (id, content, tags, confidence, `created_at`), ordered oldest first. The reported `memory_count` is the length of that live list, not a stored column. Errors if the id is unknown. |
| `list_sessions` | All optional: `agent_id`, `tag`, `finalized` (`true` = has a summary, `false` = open), `limit` (default 20), `offset`. Returns sessions newest-first by `started_at`, each with the same metadata block as `get_session` and a live non-deleted `memory_count`, in one query. |
| `finalize_session` | Takes `session_id` and optional `summary` / `tags`; sets `ended_at = time::now()` plus whichever fields were supplied. Errors if the id is unknown. |

Both `Option` fields on `finalize_session` are genuinely optional: calling it with only
`session_id` sets `ended_at` without recording a summary — which leaves the session counting as
*unfinalized*, since a summary is the only closed signal.

## Current limitations

These are real gaps in the shipped implementation, not usage advice:

- **Sessions are not reachable through `recall`** (which walks clusters, not sessions), and
  `list_sessions` has no search — it filters on `agent_id`, `tag`, and finalized state only, so
  finding a session by what its summary says means paging through the list. Browsing is the debug
  UI's other route: `/debug/sessions` (list, in the total order above) and
  `/debug/sessions/{external_id}` (detail: summary, tags, the session's memories), HTTP mode only — or
  query the `session` table directly.
- **The pi extension groups but does not finalize.** `contrib/pi/` sends pi's session id,
  `agent_id = "pi"` and the model with every auto-store, so those writes are grouped; the extension
  itself never calls `finalize_session`, so sessions created by auto-store have no summary unless the
  agent calls it directly — which the pi skill does document.

## SurrealDB 3.2 gotchas in this code path

Non-obvious, and easy to reintroduce:

- `session` is a **reserved word** in SurrealDB 3.2 — this repo backticks it in every query string,
  and the table is `SCHEMAFULL` so an undefined field silently fails.
- `$session` is **also reserved** (it is SurrealDB's own session variable). Bind parameters for a
  session record ID must use another name — `session_repo.rs` uses `$sess`.
- `RELATE` needs a pre-parsed `RecordId` passed through `.bind()`; an inline `type::record()` in a
  `RELATE` statement fails.
- `value` is reserved as well, so `SELECT value FROM <table>` fails — select `*` and read the field
  out (see the project-wide list in [AGENTS.md](../AGENTS.md)).
- The derived count has to put its filter **inside** the traversal target's parentheses:
  `(->contains_session_memory->(fact WHERE deleted = false)).len()`. The natural-looking
  `(->contains_session_memory->fact WHERE deleted = false).len()` is a parse error — `Unexpected token
  WHERE, expected delimiter )` — and so are the `array::len(...)` and `count(...)` spellings, with or
  without a bound parameter.
- `ORDER BY ended_at DESC NULLS LAST` and `ORDER BY type::coalesce(a, b) DESC` both fail to parse, so
  there is no portable way to override where NULL `ended_at` sorts. Under `DESC` it lands at the tail,
  which is where a created-but-never-touched session belongs anyway. What is *not* defined by that
  ordering is the sequence among the NULLs — hence the `external_id ASC` tiebreaker above.
