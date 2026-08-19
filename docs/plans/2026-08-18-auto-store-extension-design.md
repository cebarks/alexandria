# Alexandria Auto-Store Extension Design

**Date**: 2026-08-18
**Status**: Draft — reviewed, ready for implementation planning

## Overview

Expand the existing `alexandria-auto-recall` extension to include store-side behavior.
Currently the extension only *reads* memory (auto-recall on every prompt via
`before_agent_start`). This design adds two tiers of automatic *write* behavior:

1. **Heuristic detectors** — real-time pattern matching on user prompts and tool results,
   no LLM involvement, immediate `store_memory` calls.
2. **LLM extraction** — at `session_shutdown`, serialize the conversation and ask a cheap
   model to extract durable facts/decisions/preferences that the heuristics missed.

Together with the existing skill (`contrib/pi/skills/alexandria-memory/SKILL.md`) which
instructs the agent to proactively call `store_memory` / `update_memory` during the session,
this creates a three-layer write funnel:

| Layer | Trigger | Latency | Quality |
| ------- | --------- | --------- | --------- |
| Skill (existing) | Agent decides during conversation | Zero (inline tool call) | Highest — LLM judgment in context |
| Heuristic detectors (new) | Pattern match on prompts/tool results | Zero (fire-and-forget) | Medium — regex, no semantic understanding |
| LLM extraction (new) | `session_shutdown` | 2–5s | High — LLM judgment over full conversation |

The heuristic layer catches things the agent might not think to store (corrections,
preference statements). The extraction layer is a safety net for anything both the agent
and the heuristics missed.

## Architecture

### File structure

The extension stays as a single entry point (`src/index.ts`) but gains internal modules:

```
contrib/pi/extensions/alexandria-auto-recall/
├── src/
│   ├── index.ts              # Extension factory, event wiring
│   ├── recall.ts             # Existing auto-recall logic (extracted from index.ts)
│   ├── detectors/
│   │   ├── types.ts          # Shared detector interfaces
│   │   ├── correction.ts     # Correction pattern detector
│   │   ├── preference.ts     # Preference pattern detector
│   │   ├── error-tracker.ts  # Error→resolution pair tracker
│   │   └── tool-tracker.ts   # Dedup tracker for skill-driven stores
│   ├── extraction.ts         # LLM extraction at session_shutdown
│   └── mcp-client.ts         # Shared Alexandria MCP client (extracted from index.ts)
├── package.json
├── tsconfig.json
└── tests/                    # Unit tests for detectors + extraction prompt
```

### Data flow

```
User prompt arrives
  │
  ├─► [existing] before_agent_start → retrieve_memories → inject context
  │
  ├─► before_agent_start → correction detector → store_memory (tag: correction)
  ├─► before_agent_start → preference detector → store_memory (tag: preference)
  │
  │   Agent runs, calls tools...
  │
  ├─► tool_result → tool dedup tracker (records store_memory/update_memory calls)
  ├─► tool_execution_end → error tracker (records errors, pairs with subsequent successes)
  │
  ├─► agent_end → error tracker flushes paired resolutions → store_memory (tag: error-resolution)
  │
  └─► session_shutdown (reason != "reload")
        ├─ Serialize conversation via buildContextEntries()
        ├─ Collect "already stored" set (tool tracker + heuristic buffer)
        ├─ Call extraction model
        ├─ Parse structured JSON response
        ├─ Deduplicate against "already stored" set
        └─ store_memory for each extracted item
```

## Heuristic Detectors

### Correction Detector

**Hook**: `before_agent_start`
**Input**: `event.prompt`

Scans user prompts for correction-shaped language. Fires when unambiguous; defers
ambiguous cases to the LLM extraction pass.

**Patterns** (case-insensitive):

- `no, use X` / `no, it should be X` / `no, it's X`
- `that's wrong` / `that's incorrect` / `that's not right`
- `actually, X` / `actually it's X`
- `I meant X` / `I said X`
- `not X, Y` / `don't use X, use Y` / `use X instead of Y`
- `wrong — X` / `incorrect — X`

**Output**: Extracts the corrected statement as `content`, stores with `tags: ["correction", "auto-detected"]`.

**Dedup**: Normalized string comparison against an in-memory buffer of corrections stored
this session. Avoids firing twice on rephrased corrections of the same thing.

### Preference Detector

**Hook**: `before_agent_start`
**Input**: `event.prompt`

Scans for forward-looking preference/convention statements.

**Patterns** (case-insensitive):

- `always X` / `never X`
- `I prefer X` / `I like X better`
- `use X instead of Y` / `default to X`
- `don't ever X` / `make sure to X`
- `from now on X` / `going forward X`

**Output**: Extracts the preference statement as `content`, stores with `tags: ["preference", "auto-detected"]`.

**Dedup**: Same normalized string buffer as the correction detector.

### Tool Dedup Tracker

**Hook**: `tool_result` (not `tool_execution_end` — see review note below)
**Input**: `event.toolName`, `event.input`, `event.isError`

Watches for `store_memory` and `update_memory` tool calls made by the agent (skill-driven).
Does **not** store anything itself — only records content hashes of what was stored.

> **Review note**: The original design specified `tool_execution_end`, but that event only
> carries `result: any` — it does NOT include the original tool input/args. The `tool_result`
> event carries both `event.input: Record<string, unknown>` (the args passed to the tool)
> and `event.isError: boolean`, which is what we need to extract the `content` field from
> `store_memory` calls and skip failed stores.

**State**: `Set<string>` of SHA-256 hashes of stored `content` fields, plus a parallel
array of raw content strings (for the LLM extraction prompt's "already stored" section).

**Purpose**: Prevents the LLM extraction pass at shutdown from re-extracting facts the
agent explicitly stored during the session.

### Error Resolution Tracker

**Hook**: `tool_execution_end` (accumulate) → `agent_end` (flush)
**Input**: `event.toolName`, `event.result`, `event.isError`

> **Note**: Unlike the tool dedup tracker, `tool_execution_end` is correct here — we need
> `event.result` (the tool output) to extract error/success text, and we don't need the
> original input args.

Tracks error→success sequences across tool calls within an agent run:

1. When `event.isError === true`: record `{toolName, errorText}` in a bounded ring buffer
   (max 5 entries — prevents unbounded growth on noisy tool failures).
2. When a subsequent `tool_execution_end` succeeds for a previously-errored tool: pair
   them as a resolution.
3. At `agent_end`: flush all paired resolutions as `store_memory` calls with
   `tags: ["error-resolution", "auto-detected", toolName]`.

**Content format**:

```
Error with <toolName>: <error summary>
Resolution: <success summary>
```

Unpaired errors (no successful follow-up) are discarded — they're likely still-open
problems, not resolved learnings.

## LLM Extraction at Session Shutdown

### Trigger

`session_shutdown` event, **skipped when** `event.reason === "reload"`.

Runs on: `quit`, `new`, `resume`, `fork` — these all represent meaningful conversation
boundaries where extraction is valuable.

### Conversation serialization

```typescript
const entries = ctx.sessionManager.buildContextEntries();
const serialized = serializeForExtraction(entries);
```

`buildContextEntries()` returns the compacted branch — summary + recent messages. This is
already trimmed to a reasonable size and won't blow up the extraction model's context.

The serialization strips tool call details and binary content, keeping only:

- User messages (full text)
- Assistant messages (text only, no tool calls)
- Compaction summaries
- A rough turn structure (turn N: user said X, assistant said Y)

### Extraction prompt

```
You are a memory extraction system. Given a conversation between a user and an AI
coding assistant, extract durable facts worth remembering across sessions.

Extract:
- User preferences and conventions (tooling choices, style rules, workflow habits)
- Architectural/design decisions AND their rationale
- Bug root causes once resolved (not symptoms)
- Non-obvious gotchas, footguns, or platform/library quirks
- Corrections the user gave about something the assistant got wrong

Do NOT extract:
- Ephemeral task details (file paths being edited, current branch name, etc.)
- Things already in the "already stored" list below
- Common knowledge or well-documented behavior
- Incomplete work or open questions

Each extracted memory must be a standalone statement that makes sense without this
conversation. No "as discussed above", no pronouns without antecedents.

Already stored this session (do not duplicate):
<already_stored>
{list of content strings from tool tracker + heuristic buffer}
</already_stored>

Conversation:
<conversation>
{serialized conversation}
</conversation>

Respond with JSON only:
{
  "memories": [
    {"content": "standalone statement", "tags": ["relevant", "tags"]},
    ...
  ]
}

If nothing is worth extracting, respond with: {"memories": []}
```

### Model configuration

| Env var | Default | Description |
|---------|---------|-------------|
| `ALEXANDRIA_EXTRACT_MODEL` | `vertex/claude-haiku-4-5` | `provider/model-id` format |
| `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | `5000` | Total timeout for the extraction call |

The provider/model string is split on the first `/`. The provider ID is used with
`ctx.modelRegistry.getProviderAuth(providerId)` to resolve API key, base URL, and headers.

The model call is a single non-streaming chat completion via raw `fetch` against the
provider's API endpoint. Response is parsed as JSON with a try/catch fallback
(malformed response → warn and skip, don't crash).

### Failure handling

- **Model call fails**: warn via `ctx.ui.notify()`, continue shutdown. Never block exit.
- **JSON parse fails**: warn, skip. Don't retry — it's a best-effort safety net.
- **Alexandria server unreachable**: warn, skip (same as existing recall behavior).
- **Timeout**: `AbortController` with the configured timeout. On abort, warn and skip.

## Deduplication Strategy

Three levels, all intra-session only. No cross-session dedup — let Alexandria's clustering
handle semantic overlap across sessions.

| Level | Mechanism | Purpose |
| ------- | ----------- | --------- |
| Heuristic → heuristic | Normalized string comparison (lowercase, collapse whitespace) | Same correction/preference detected twice in one session |
| Agent tool calls → extraction | Content hash set (SHA-256) | Extraction pass doesn't re-extract what the agent explicitly stored |
| Heuristic stores → extraction | Raw content strings in extraction prompt | Extraction pass sees what heuristics already stored |

## Configuration

All configuration via environment variables, consistent with the existing extension:

| Variable | Default | Description |
| ---------- | --------- | ------------- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria MCP server URL |
| `ALEXANDRIA_AUTO_RECALL` | (enabled) | Set to `off` to disable auto-recall |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories to inject per prompt |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.5` | Similarity threshold for recall |
| `ALEXANDRIA_AUTO_STORE` | (enabled) | Set to `off` to disable all store-side behavior |
| `ALEXANDRIA_EXTRACT_MODEL` | `vertex/claude-haiku-4-5` | Extraction model (`provider/model-id`) |
| `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | `5000` | Extraction call timeout |

## Testing

### Unit tests (new `tests/` directory)

- **Correction detector**: test each regex pattern against positive/negative examples
- **Preference detector**: same pattern coverage
- **Error tracker**: test pairing logic, ring buffer bounds, flush behavior
- **Tool tracker**: test hash recording, dedup set membership
- **Extraction prompt builder**: test serialization, "already stored" injection
- **Extraction response parser**: test valid JSON, malformed JSON, empty response

### Integration tests

- **Full flow**: mock Alexandria MCP server, simulate a session with corrections/preferences,
  verify `store_memory` calls match expectations
- **Extraction flow**: mock both Alexandria and the extraction model, verify end-to-end
- **Failure modes**: Alexandria down, model timeout, malformed model response

### Manual testing

- Run a pi session with the extension loaded, make corrections and preferences, verify
  memories appear in Alexandria after session ends
- Check that duplicate stores from skill + heuristic + extraction don't create excessive noise

## Migration / Backward Compatibility

- The existing `before_agent_start` auto-recall behavior is unchanged
- All new behavior is additive and behind `ALEXANDRIA_AUTO_STORE` (enabled by default)
- The extension's `package.json` gains no new dependencies (uses `fetch` and existing
  `@modelcontextprotocol/client`)
- The `src/index.ts` refactor extracts recall logic to `recall.ts` but the external
  behavior is identical

## Open Questions

1. **Extraction model auth**: `getProviderAuth()` requires the provider to be configured in
   pi's model registry. If `vertex` isn't configured, extraction silently fails. Should we
   fall back to `ctx.model` (the session's active model) if the configured extraction
   provider isn't available? **Decision: yes** — fall back to `ctx.model` if the configured
   provider is unavailable. More expensive but better than silent failure. Log a warning
   when falling back so the user knows to configure the extraction provider.

2. **Conversation size**: `buildContextEntries()` after compaction should be manageable, but
   a long session without compaction could produce a large payload. Should we cap the
   serialized conversation at a token budget (e.g., 8k tokens for Haiku's 200k context)?
   **Decision: yes** — cap at 16k tokens (generous for Haiku 4.5's 200k context). Truncate
   from the oldest messages first, keeping the compaction summary + most recent turns.

3. **Rate of heuristic false positives**: The regex patterns will inevitably match some
   non-correction/preference statements. The bias is intentionally toward storing (cheap,
   clustering handles dedup). Monitor in practice and tighten patterns if noise is excessive.
   **Decision: accepted risk** — bias toward storing, iterate on patterns.

## Review Notes

**Reviewed**: 2026-08-18, self-review against pi extension API type definitions.

**Findings**:

1. **Tool dedup tracker must use `tool_result`, not `tool_execution_end`**.
   `ToolExecutionEndEvent` has `{ toolCallId, toolName, result, isError }` — no `input`
   field. `ToolResultEvent` has `{ toolCallId, toolName, input, content, isError }` —
   includes the original args, which is what we need to extract the `content` field from
   `store_memory` calls. Spec updated.

2. **Error tracker correctly uses `tool_execution_end`**. It needs `result` (tool output)
   to extract error/success text, and doesn't need input args.

3. **All other hooks verified correct** against the type definitions:
   - `BeforeAgentStartEvent.prompt: string` ✅
   - `AgentEndEvent.messages: AgentMessage[]` ✅
   - `SessionShutdownEvent.reason: "quit" | "reload" | "new" | "resume" | "fork"` ✅
   - `ToolExecutionEndEvent { toolCallId, toolName, result, isError }` ✅

4. **`ctx.sessionManager.buildContextEntries()` availability at shutdown**: The docs say
   `session_shutdown` fires before teardown, and `ctx.sessionManager` is accessible in
   all event contexts. Verified viable.

5. **`ctx.modelRegistry.getProviderAuth()` at shutdown**: Available via `ctx` in all
   handlers. No lifecycle concern.

6. **Async work in `session_shutdown`**: The handler is `async` and pi awaits it before
   proceeding with shutdown. The `AbortController` timeout in the spec is the right
   safeguard to avoid blocking exit indefinitely.

7. **MCP tool names**: The spec references `store_memory` and `update_memory` as tool
   names to watch in `tool_result`. These are the actual MCP tool names registered by
   Alexandria. When called via pi's MCP integration, `event.toolName` will be the
   server-prefixed name (e.g., `alexandria_store_memory` if the MCP server prefix is
   `alexandria`). The implementation must account for the MCP server prefix configured
   in `mcp.json` — either match by suffix or read the prefix from config.
