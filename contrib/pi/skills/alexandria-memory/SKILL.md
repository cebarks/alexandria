---
name: alexandria-memory
description: Use the Alexandria agent-memory MCP tools (alexandria_store_memory, alexandria_retrieve_memories, alexandria_recall, alexandria_update_memory, alexandria_import_document, alexandria_delete_memory) to persist and recall durable facts, decisions, and preferences across sessions. Use PROACTIVELY at the start of tasks in known projects/domains, whenever the user references past context ("last time", "we decided", "like before"), and immediately after learning something worth keeping (a preference, an architectural decision + rationale, a bug's root cause, a correction) — not only when explicitly asked to remember or recall.
---

# Alexandria Memory

Alexandria is a persistent, cross-session agent memory server (semantic search + heat-based
recency + graph clustering) reachable via MCP tools when the `alexandria` server is connected.
Its whole value only materializes if it's actually used — an agent that never calls it behaves
exactly like one with no memory at all. Default to using it; don't wait for an explicit
"remember this" / "check your memory" instruction.

## When to read memory (do this unprompted)

- Starting a task in a project/domain you've plausibly touched before (early in the session,
  before diving into research you might have already done).
- The user references past context: "last time", "we decided", "like before", "you said",
  "remind me".
- Before re-deriving an architectural decision, re-debugging something, or re-asking a
  preference question the user may have already answered in a prior session.
- Before proposing an approach that has tradeoffs — check whether a prior decision/rationale
  already exists so you don't contradict it silently.

Tool choice:
- **`alexandria_retrieve_memories`** — specific lookup, you know roughly what you're searching for. Pass a
  natural-language statement of the fact/topic (not a question). Returns ranked hits with
  similarity + tags.
- **`alexandria_recall`** — open-ended/broad exploration ("what do we know about X", "what's the state of
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
- **`alexandria_store_memory`** — new fact. Write `content` as a standalone statement that still makes
  sense without today's conversation (no "as discussed above", no pronouns without antecedents).
  Add `tags` for the project/domain so future retrieval scopes well.
- **`alexandria_update_memory`** — you found that an *existing* memory is stale/wrong. Prefer this over
  `alexandria_store_memory` for corrections — it re-embeds if content changed and preserves the old
  version via a `derived_from` lineage edge instead of leaving a stale duplicate floating
  around.
- **`alexandria_import_document`** — bulk reference material (a design doc, README, spec, meeting notes)
  the user shares or points at that's worth retaining long-term. Chunks automatically
  (heading/paragraph/fixed-size) and links chunks back to the source document.
- **`alexandria_delete_memory`** — only when the user explicitly wants something forgotten. This is a
  soft-delete; for corrections, prefer `alexandria_update_memory` so the lineage survives.

## Guidelines

- **Bias toward storing.** Memory writes are cheap and clustering handles rough dedup; losing a
  decision/rationale is more costly than an extra memory entry.
- **Write for a stranger.** Assume the reader has zero context from this conversation.
- **Don't narrate it.** Store/retrieve silently as part of normal work, the same way you'd read
  a file — don't announce "let me check my memory" unless the result changes what you tell the
  user.
- **Surface conflicts.** If a retrieved memory contradicts what you're about to do or say, flag
  it to the user rather than silently picking one.
- **Batch queries when exploring multiple angles** — call `retrieve_memories` a few times with
  varied phrasing rather than one query trying to cover everything, same principle as varying
  web-search queries.

## Availability

These tools come from the `alexandria` MCP server (HTTP, `http://127.0.0.1:3000/mcp`, configured
with `keep-alive` lifecycle). If the tools aren't present, the server may not be running —
check `systemctl --user status alexandria` (if deployed as a systemd user service) before
concluding memory isn't available for this session.
