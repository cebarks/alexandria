# Session Memory

Memories are grouped into **sessions** — an optional, client-named bucket that lets an agent
answer "what did we learn in that conversation?" and "what do I know from *just* this
conversation?" without re-deriving either from tags.

Sessions are opt-in per call. Nothing in Alexandria requires them, and memories stored without
one behave exactly as they always have.

## Data model

Schema `v005_session.surql`, amended by `v006_drop_session_memory_count.surql` and account-qualified by `v010_scoped_sessions.surql`:

```text
session
├── account        record       -- owner; external_id is unique within this account
├── workspace      record
├── external_id    string       -- client-supplied handle
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
are **derived** from the owned `contains_session_memory` traversal. In scoped views, counts follow
live fact eligibility and the resolved partition, not an archive-wide tally.

`external_id` is the only identity that matters to clients. It is an opaque string the caller
chooses (pi's session UUID, a ticket number, `"2026-08-27-auth-refactor"` — anything), and it is
what you pass to every session tool below. Two accounts can reuse the same handle without sharing
anything. SurrealDB's account-qualified record key is internal. Legacy adoption re-keys sessions and
preserves their links for the nominated owner.

## Lifecycle

```text
store_memory(content="…", scope="personal", session_id="s1") # create `s1`, link new fact
store_memory(content="…", scope="personal", session_id="s1") # add another link
retrieve_memories(query="…", session_id="s1") # within session AND resolved partitions
get_session(session_id="s1")                 # scoped memories, no archive summary/tags
get_session(session_id="s1", view="archive") # private summary/tags, no memory bodies
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
sessions land in the tail. `external_id` breaks ties because it is unique within the account being listed. Ordering by `ended_at` alone left the ties unspecified, and because the debug
UI pages with `LIMIT`/`START` that is a correctness bug, not a cosmetic one — page 2 could repeat a
session page 1 showed while silently dropping another.

## Tools

| Tool | Session behavior |
| --- | --- |
| `store_memory` | Optional `session_id`. Auto-creates the session and relates the new fact to it. The create is a guarded write: if the caller's credential is revoked between claim and commit, the transaction conflicts and no session row appears. |
| `retrieve_memories` | Optional `session_id` narrows the candidate set to that session's facts before ranking. It narrows, it does not widen: the rows are read through the caller's **resolved partitions**, so an `auto` read with no project over a session that also holds project rows answers with the personal ones. |
| `get_session` | Takes `session_id`, plus `project` and `view` — there is no `scope` parameter on either session tool. Default `view: "scoped"` returns the session's memories (id, content, tags, confidence, `created_at`) **inside the resolved population**, oldest first, with `summary` and `tags` withheld and `memory_count` counted in that same population. `view: "archive"` is the deliberate whole-session read: the owner's own prose included, across the owned scopes, and no memory body at all. Errors if the id is unknown, and an unknown id — including somebody else's — answers the same way. |
| `list_sessions` | All optional: `view`, `project`, `agent_id`, `tag`, `finalized` (`true` = has a summary, `false` = open), `limit` (default 20), `offset`. Returns sessions newest-first by `ended_at` (then `external_id`, the tie-break that makes the order total — see above), each with a live non-deleted `memory_count` counted inside the resolved population. `tag` is accepted only with `view: "archive"`; asking a scoped page to filter by prose it does not expose is refused rather than quietly answered with nothing. |
| `finalize_session` | Takes `session_id` and optional `summary` / `tags`; sets `ended_at = time::now()` plus whichever fields were supplied. Errors if the id is unknown. |

`view` is a separate decision from `session_id`: a scoped read never exposes archive summary/tags.
An explicit archive read is account-private, not a way to retrieve fact bodies across projects.

Both `Option` fields on `finalize_session` are genuinely optional: calling it with only
`session_id` sets `ended_at` without recording a summary — which leaves the session counting as
*unfinalized*, since a summary is the only closed signal.

## Current limitations

These are real gaps in the shipped implementation, not usage advice:

- **Sessions are not reachable through `recall`** (which walks clusters, not sessions), and
  `list_sessions` has no search — it filters on `agent_id`, `tag`, and finalized state only, so
  finding a session by what its summary says means paging through the list. Browsing is the debug
  UI's other route: `/debug/sessions` and its owner-scoped detail, in HTTP mode.
- **Clients do not auto-store or finalize.** An agent can opt into grouping with a client session ID
  on explicit stores, and explicitly finalize a useful session. Recall does not implicitly read the
  archive.

## SurrealDB gotchas in this code path

Non-obvious, and easy to reintroduce:

- The `session` table is `SCHEMAFULL`, so writing an undefined field is an error rather than being
  stored. (`session` itself is not a reserved word; the backticks in every query string are
  harmless.)
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
