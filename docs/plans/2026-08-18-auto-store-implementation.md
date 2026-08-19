# Auto-Store Extension Implementation Plan

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Expand the existing `alexandria-auto-recall` extension with store-side behavior — heuristic detectors for corrections/preferences/error-resolutions, plus LLM extraction at session shutdown.

**Architecture:** Refactor the monolithic `src/index.ts` into modules: shared MCP client, recall logic, four detectors, and an LLM extraction pass. The extension factory wires hooks to modules. All new state is session-scoped (in-memory), all stores go through the existing MCP client to Alexandria.

**Tech Stack:** TypeScript (jiti-loaded by pi), `@modelcontextprotocol/client` (existing dep), pi extension API (`before_agent_start`, `tool_result`, `tool_execution_end`, `agent_end`, `session_shutdown`), `ctx.modelRegistry.complete()` for the extraction model call.

**Design spec:** `docs/plans/2026-08-18-auto-store-extension-design.md`

---

### Task 1: Extract MCP Client to Shared Module

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/mcp-client.ts`
- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/index.ts`

**Step 1: Create `mcp-client.ts` with the shared client**

```typescript
// src/mcp-client.ts
import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";

const SERVER_URL = process.env.ALEXANDRIA_URL ?? "http://127.0.0.1:3000/mcp";

let clientPromise: Promise<Client> | null = null;

export async function getClient(): Promise<Client> {
 if (!clientPromise) {
  clientPromise = (async () => {
   const client = new Client({ name: "alexandria-auto-recall", version: "2.0.0" });
   const transport = new StreamableHTTPClientTransport(new URL(SERVER_URL));
   await client.connect(transport);
   return client;
  })();
 }
 return clientPromise;
}

export function resetClient(): void {
 clientPromise = null;
}

export async function closeClient(): Promise<void> {
 if (clientPromise) {
  try {
   const client = await clientPromise;
   await client.close();
  } catch {
   // best-effort cleanup
  }
  clientPromise = null;
 }
}

export function extractTextContent(content: unknown): string | undefined {
 if (!Array.isArray(content)) return undefined;
 for (const block of content) {
  if (block && typeof block === "object" && "type" in block && block.type === "text" && "text" in block) {
   return String((block as { text: unknown }).text);
  }
 }
 return undefined;
}

export async function storeMemory(content: string, tags: string[]): Promise<void> {
 const client = await getClient();
 await client.callTool({
  name: "store_memory",
  arguments: { content, tags },
 });
}
```

**Step 2: Update `index.ts` to import from `mcp-client.ts`**

Replace the inline client code in `index.ts` with imports from `mcp-client.ts`. Keep the
`retrieveMemories`, `formatMemoriesBlock`, and the `before_agent_start` / `session_shutdown`
hooks in `index.ts` for now — recall logic moves in Task 2.

The `session_shutdown` handler should call `closeClient()` from the shared module.

**Step 3: Run `pi -e contrib/pi/extensions/alexandria-auto-recall/src/index.ts` briefly to verify import resolution works**

Expected: Extension loads without errors. (Alexandria server doesn't need to be running — the client connects lazily.)

**Step 4: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/
git commit -m "refactor(ext): extract MCP client to shared module"
```

---

### Task 2: Extract Recall Logic to Its Own Module

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/recall.ts`
- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/index.ts`

**Step 1: Create `recall.ts`**

Move `retrieveMemories()`, `formatMemoriesBlock()`, and the `RetrievedMemory` /
`RetrieveMemoriesResponse` interfaces out of `index.ts` into `recall.ts`.

```typescript
// src/recall.ts
import { getClient, extractTextContent } from "./mcp-client.js";

const RESULT_LIMIT = Number(process.env.ALEXANDRIA_AUTO_RECALL_LIMIT ?? "5");
const MIN_SIMILARITY = Number(process.env.ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY ?? "0.5");

interface RetrievedMemory {
 id: string;
 content: string;
 similarity: number;
 tags?: string[];
}

interface RetrieveMemoriesResponse {
 results?: RetrievedMemory[];
 error?: string;
}

export async function retrieveMemories(query: string): Promise<RetrievedMemory[]> {
 const client = await getClient();
 const result = await client.callTool({
  name: "retrieve_memories",
  arguments: { query, limit: RESULT_LIMIT },
 });

 const text = extractTextContent(result.content);
 if (!text) return [];

 let parsed: RetrieveMemoriesResponse;
 try {
  parsed = JSON.parse(text) as RetrieveMemoriesResponse;
 } catch {
  return [];
 }

 return (parsed.results ?? []).filter((m) => m.similarity >= MIN_SIMILARITY);
}

export function formatMemoriesBlock(memories: RetrievedMemory[]): string {
 const lines = memories.map((m) => {
  const tags = m.tags && m.tags.length > 0 ? ` [${m.tags.join(", ")}]` : "";
  return `- (similarity ${m.similarity.toFixed(2)}, id ${m.id})${tags} ${m.content}`;
 });
 return [
  "Relevant memories retrieved automatically from Alexandria for this prompt:",
  ...lines,
  "",
  "These are surfaced proactively; verify relevance before relying on them, and use update_memory if any is stale.",
 ].join("\n");
}
```

**Step 2: Slim down `index.ts`**

`index.ts` becomes the wiring layer — imports recall and delegates:

```typescript
// src/index.ts (after refactor — recall portion)
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { resetClient, closeClient } from "./mcp-client.js";
import { retrieveMemories, formatMemoriesBlock } from "./recall.js";

const RECALL_DISABLED = process.env.ALEXANDRIA_AUTO_RECALL === "off";

export default function alexandriaExtension(pi: ExtensionAPI) {
 if (!RECALL_DISABLED) {
  pi.on("before_agent_start", async (event, ctx) => {
   const query = event.prompt?.trim();
   if (!query) return;

   try {
    const memories = await retrieveMemories(query);
    if (memories.length === 0) return;

    return {
     message: {
      customType: "alexandria-auto-recall",
      content: formatMemoriesBlock(memories),
      display: true,
     },
    };
   } catch (err) {
    resetClient();
    ctx.ui.notify(
     `Alexandria auto-recall failed (${err instanceof Error ? err.message : String(err)}); continuing without it.`,
     "warning",
    );
    return;
   }
  });
 }

 pi.on("session_shutdown", async () => {
  await closeClient();
 });
}
```

**Step 3: Verify extension loads cleanly**

Run: `pi -e contrib/pi/extensions/alexandria-auto-recall/src/index.ts`
Expected: Extension loads, recall behavior identical to before.

**Step 4: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/
git commit -m "refactor(ext): extract recall logic to recall.ts"
```

---

### Task 3: Add Config and Shared Types

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/config.ts`
- Create: `contrib/pi/extensions/alexandria-auto-recall/src/detectors/types.ts`
- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/index.ts` (import config)
- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/mcp-client.ts` (import config)
- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/recall.ts` (import config)

**Step 1: Create `config.ts`**

Centralize all env var reads:

```typescript
// src/config.ts
export const CONFIG = {
 serverUrl: process.env.ALEXANDRIA_URL ?? "http://127.0.0.1:3000/mcp",
 recallDisabled: process.env.ALEXANDRIA_AUTO_RECALL === "off",
 recallLimit: Number(process.env.ALEXANDRIA_AUTO_RECALL_LIMIT ?? "5"),
 recallMinSimilarity: Number(process.env.ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY ?? "0.5"),
 storeDisabled: process.env.ALEXANDRIA_AUTO_STORE === "off",
 extractModel: process.env.ALEXANDRIA_EXTRACT_MODEL ?? "vertex/claude-haiku-4-5",
 extractTimeoutMs: Number(process.env.ALEXANDRIA_EXTRACT_TIMEOUT_MS ?? "5000"),
} as const;
```

**Step 2: Create `detectors/types.ts`**

Shared interfaces for all detectors:

```typescript
// src/detectors/types.ts

/** A memory candidate detected by a heuristic or the LLM extraction pass. */
export interface DetectedMemory {
 content: string;
 tags: string[];
}

/**
 * Session-scoped dedup buffer.
 * Tracks what has been stored this session (by both heuristics and agent tool calls)
 * to prevent the LLM extraction pass from re-extracting known facts.
 */
export class SessionDedupBuffer {
 private readonly normalizedStrings = new Set<string>();
 private readonly contentHashes = new Set<string>();
 private readonly rawContents: string[] = [];

 private normalize(s: string): string {
  return s.toLowerCase().replace(/\s+/g, " ").trim();
 }

 private async hash(s: string): Promise<string> {
  const data = new TextEncoder().encode(s);
  const buf = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(buf)).map(b => b.toString(16).padStart(2, "0")).join("");
 }

 /** Record a heuristic-detected store (normalized string dedup). */
 addHeuristicStore(content: string): boolean {
  const norm = this.normalize(content);
  if (this.normalizedStrings.has(norm)) return false;
  this.normalizedStrings.add(norm);
  this.rawContents.push(content);
  return true;
 }

 /** Record an agent-initiated store_memory call (content hash dedup). */
 async addToolStore(content: string): Promise<void> {
  const h = await this.hash(content);
  this.contentHashes.add(h);
  this.rawContents.push(content);
 }

 /** Check if content was already stored by a heuristic this session. */
 hasHeuristic(content: string): boolean {
  return this.normalizedStrings.has(this.normalize(content));
 }

 /** Get all raw content strings for the extraction prompt's "already stored" section. */
 getAllStoredContents(): readonly string[] {
  return this.rawContents;
 }
}
```

**Step 3: Update `mcp-client.ts` and `recall.ts` to use `CONFIG`**

Replace inline `process.env` reads with imports from `config.ts`.

**Step 4: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/
git commit -m "feat(ext): add centralized config and shared dedup buffer types"
```

---

### Task 4: Correction Detector

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/detectors/correction.ts`

**Step 1: Write the correction detector**

```typescript
// src/detectors/correction.ts
import type { SessionDedupBuffer, DetectedMemory } from "./types.js";

/**
 * Patterns that indicate the user is correcting the agent.
 * Each pattern captures the corrected statement in group 1.
 * Order matters: more specific patterns first.
 */
const CORRECTION_PATTERNS: RegExp[] = [
 /\bno[,.]?\s+(?:use|it\s+should\s+be|it'?s)\s+(.+)/i,
 /\bthat'?s\s+(?:wrong|incorrect|not\s+right)[,.]?\s*(.+)/i,
 /\bactually[,.]?\s+(.+)/i,
 /\bi\s+meant\s+(.+)/i,
 /\bnot\s+.{2,30}[,;]\s*(?:use|it'?s)\s+(.+)/i,
 /\bdon'?t\s+use\s+.{2,30}[,;]\s*use\s+(.+)/i,
 /\buse\s+(.+?)\s+instead\s+of\s+.+/i,
 /\bwrong\s*[—–-]\s*(.+)/i,
 /\bincorrect\s*[—–-]\s*(.+)/i,
];

/**
 * Scan a user prompt for correction patterns.
 * Returns a DetectedMemory if an unambiguous correction is found, or null.
 */
export function detectCorrection(prompt: string, buffer: SessionDedupBuffer): DetectedMemory | null {
 const trimmed = prompt.trim();
 // Skip very short or very long prompts — corrections are conversational, not essays
 if (trimmed.length < 8 || trimmed.length > 500) return null;

 for (const pattern of CORRECTION_PATTERNS) {
  const match = trimmed.match(pattern);
  if (match?.[1]) {
   const correctedFact = match[1].replace(/[.!]+$/, "").trim();
   if (correctedFact.length < 5) continue; // too short to be useful

   // Build standalone content
   const content = `User correction: ${correctedFact}`;

   if (!buffer.addHeuristicStore(content)) return null; // already stored this session

   return {
    content,
    tags: ["correction", "auto-detected"],
   };
  }
 }

 return null;
}
```

**Step 2: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/detectors/correction.ts
git commit -m "feat(ext): add correction heuristic detector"
```

---

### Task 5: Preference Detector

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/detectors/preference.ts`

**Step 1: Write the preference detector**

```typescript
// src/detectors/preference.ts
import type { SessionDedupBuffer, DetectedMemory } from "./types.js";

const PREFERENCE_PATTERNS: RegExp[] = [
 /\balways\s+(.+)/i,
 /\bnever\s+(.+)/i,
 /\bi\s+prefer\s+(.+)/i,
 /\bi\s+like\s+(.+?)\s+better/i,
 /\bdefault\s+to\s+(.+)/i,
 /\bdon'?t\s+ever\s+(.+)/i,
 /\bmake\s+sure\s+to\s+(.+)/i,
 /\bfrom\s+now\s+on[,.]?\s+(.+)/i,
 /\bgoing\s+forward[,.]?\s+(.+)/i,
 /\buse\s+(.+?)\s+instead\s+of\s+(.+)/i, // captures both sides
];

export function detectPreference(prompt: string, buffer: SessionDedupBuffer): DetectedMemory | null {
 const trimmed = prompt.trim();
 if (trimmed.length < 8 || trimmed.length > 500) return null;

 for (const pattern of PREFERENCE_PATTERNS) {
  const match = trimmed.match(pattern);
  if (match?.[1]) {
   let statement: string;
   if (match[2]) {
    // "use X instead of Y" pattern
    statement = `Use ${match[1].trim()} instead of ${match[2].replace(/[.!]+$/, "").trim()}`;
   } else {
    statement = match[1].replace(/[.!]+$/, "").trim();
   }
   if (statement.length < 5) continue;

   const content = `User preference: ${statement}`;

   if (!buffer.addHeuristicStore(content)) return null;

   return {
    content,
    tags: ["preference", "auto-detected"],
   };
  }
 }

 return null;
}
```

**Step 2: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/detectors/preference.ts
git commit -m "feat(ext): add preference heuristic detector"
```

---

### Task 6: Tool Dedup Tracker

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/detectors/tool-tracker.ts`

**Step 1: Write the tool dedup tracker**

The tracker watches `tool_result` events for `store_memory` / `update_memory` calls.
MCP tool names are server-prefixed — the pi MCP config uses `toolPrefix: "server"` and
the server key is `alexandria`, so tool names are `alexandria_store_memory` etc.
Match by suffix to be resilient to prefix changes.

```typescript
// src/detectors/tool-tracker.ts
import type { SessionDedupBuffer } from "./types.js";

const STORE_TOOL_SUFFIXES = ["store_memory", "update_memory"];

interface ToolResultLike {
 toolName: string;
 input: Record<string, unknown>;
 isError: boolean;
}

function isStoreToolCall(toolName: string): boolean {
 return STORE_TOOL_SUFFIXES.some(suffix => toolName.endsWith(suffix));
}

/**
 * If this tool_result is a successful store_memory or update_memory call,
 * record the content in the dedup buffer. Returns true if recorded.
 */
export async function trackToolStore(event: ToolResultLike, buffer: SessionDedupBuffer): Promise<boolean> {
 if (!isStoreToolCall(event.toolName)) return false;
 if (event.isError) return false;

 const content = event.input.content;
 if (typeof content !== "string" || content.length === 0) return false;

 await buffer.addToolStore(content);
 return true;
}
```

**Step 2: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/detectors/tool-tracker.ts
git commit -m "feat(ext): add tool dedup tracker for agent-initiated stores"
```

---

### Task 7: Error Resolution Tracker

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/detectors/error-tracker.ts`

**Step 1: Write the error resolution tracker**

```typescript
// src/detectors/error-tracker.ts
import type { DetectedMemory } from "./types.js";

interface ErrorRecord {
 toolName: string;
 errorText: string;
 timestamp: number;
}

interface Resolution {
 error: ErrorRecord;
 successText: string;
}

const MAX_ERRORS = 5;

/**
 * Tracks error→success sequences across tool calls.
 * Call `recordError` on tool_execution_end when isError=true.
 * Call `recordSuccess` on tool_execution_end when isError=false.
 * Call `flush` at agent_end to emit paired resolutions as DetectedMemory[].
 */
export class ErrorTracker {
 private errors: ErrorRecord[] = [];
 private resolutions: Resolution[] = [];

 recordError(toolName: string, result: unknown): void {
  const errorText = this.summarize(result);
  if (!errorText) return;

  // Ring buffer — drop oldest if full
  if (this.errors.length >= MAX_ERRORS) {
   this.errors.shift();
  }

  this.errors.push({ toolName, errorText, timestamp: Date.now() });
 }

 recordSuccess(toolName: string, result: unknown): void {
  // Find a matching error for this tool
  const errorIdx = this.errors.findIndex(e => e.toolName === toolName);
  if (errorIdx === -1) return;

  const error = this.errors[errorIdx];
  this.errors.splice(errorIdx, 1);

  const successText = this.summarize(result);
  if (!successText) return;

  this.resolutions.push({ error, successText });
 }

 /** Flush all paired resolutions as DetectedMemory[]. Clears internal state. */
 flush(): DetectedMemory[] {
  const memories = this.resolutions.map(r => ({
   content: `Error with ${r.error.toolName}: ${r.error.errorText}\nResolution: ${r.successText}`,
   tags: ["error-resolution", "auto-detected", r.error.toolName],
  }));

  this.resolutions = [];
  this.errors = [];
  return memories;
 }

 private summarize(result: unknown): string | null {
  if (typeof result === "string") {
   return result.slice(0, 300);
  }
  if (result && typeof result === "object") {
   const text = (result as Record<string, unknown>).text ??
    (result as Record<string, unknown>).message ??
    (result as Record<string, unknown>).error;
   if (typeof text === "string") return text.slice(0, 300);
   // For tool_execution_end, result is `any` — try JSON
   try {
    return JSON.stringify(result).slice(0, 300);
   } catch {
    return null;
   }
  }
  return null;
 }
}
```

**Step 2: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/detectors/error-tracker.ts
git commit -m "feat(ext): add error resolution tracker"
```

---

### Task 8: LLM Extraction Module

**Files:**

- Create: `contrib/pi/extensions/alexandria-auto-recall/src/extraction.ts`

**Step 1: Write the extraction module**

This module handles the session_shutdown LLM extraction pass. It uses
`ctx.modelRegistry.find()` + `ctx.modelRegistry.complete()` to call the extraction
model through pi's model registry — this handles all auth and API differences
(Vertex OAuth, Anthropic keys, etc.) without any provider-specific HTTP code.

```typescript
// src/extraction.ts
import { CONFIG } from "./config.js";
import type { SessionDedupBuffer, DetectedMemory } from "./detectors/types.js";

const EXTRACTION_PROMPT = `You are a memory extraction system. Given a conversation between a user and an AI coding assistant, extract durable facts worth remembering across sessions.

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

Each extracted memory must be a standalone statement that makes sense without this conversation. No "as discussed above", no pronouns without antecedents.

Respond with JSON only:
{
  "memories": [
    {"content": "standalone statement", "tags": ["relevant", "tags"]},
    ...
  ]
}

If nothing is worth extracting, respond with: {"memories": []}`;

interface ExtractionResult {
 memories: DetectedMemory[];
}

/**
 * Serialize session entries into a text representation for the extraction prompt.
 * Strips tool call details, keeps user/assistant text + compaction summaries.
 */
function serializeEntries(entries: unknown[]): string {
 const lines: string[] = [];
 let turnNum = 0;

 for (const entry of entries) {
  const e = entry as Record<string, unknown>;
  if (e.type === "message") {
   const msg = e.message as Record<string, unknown> | undefined;
   if (!msg) continue;

   const role = msg.role as string;
   const content = msg.content;

   if (role === "user") {
    turnNum++;
    const text = typeof content === "string" ? content :
     Array.isArray(content) ? content
      .filter((b: any) => b?.type === "text")
      .map((b: any) => b.text)
      .join("\n") : "";
    if (text) lines.push(`[Turn ${turnNum} - User]: ${text}`);
   } else if (role === "assistant") {
    const text = typeof content === "string" ? content :
     Array.isArray(content) ? content
      .filter((b: any) => b?.type === "text")
      .map((b: any) => b.text)
      .join("\n") : "";
    if (text) lines.push(`[Turn ${turnNum} - Assistant]: ${text}`);
   }
  } else if (e.type === "compaction") {
   const summary = (e as any).summary ?? (e as any).compaction?.summary;
   if (typeof summary === "string") {
    lines.push(`[Session Summary]: ${summary}`);
   }
  }
 }

 return lines.join("\n\n");
}

/**
 * Build the full extraction prompt with conversation and "already stored" context.
 */
function buildPrompt(serializedConversation: string, buffer: SessionDedupBuffer): string {
 const alreadyStored = buffer.getAllStoredContents();
 const alreadyStoredBlock = alreadyStored.length > 0
  ? alreadyStored.map(c => `- ${c}`).join("\n")
  : "(nothing stored yet this session)";

 return `${EXTRACTION_PROMPT}

Already stored this session (do not duplicate):
<already_stored>
${alreadyStoredBlock}
</already_stored>

Conversation:
<conversation>
${serializedConversation}
</conversation>`;
}

/**
 * Run the LLM extraction pass.
 *
 * Uses ctx.modelRegistry.find() + ctx.modelRegistry.complete() to route through
 * pi's model infrastructure — handles Vertex OAuth, Anthropic API keys, etc.
 *
 * Falls back to ctx.model (the session's active model) if the configured extraction
 * model/provider is not available.
 */
export async function runExtraction(
 ctx: {
  sessionManager: { buildContextEntries(): unknown[] };
  modelRegistry: {
   find(provider: string, modelId: string): unknown | undefined;
   complete(model: any, context: any): Promise<any>;
  };
  model: any;
  ui: { notify(msg: string, level: string): void };
 },
 buffer: SessionDedupBuffer,
): Promise<DetectedMemory[]> {
 // Serialize conversation
 const entries = ctx.sessionManager.buildContextEntries();
 const serialized = serializeEntries(entries);

 // Skip extraction if conversation is trivially short
 if (serialized.length < 100) return [];

 // Cap serialized conversation at ~16k tokens (~64k chars for safety)
 const maxChars = 64_000;
 const truncated = serialized.length > maxChars
  ? serialized.slice(serialized.length - maxChars)
  : serialized;

 const userMessage = buildPrompt(truncated, buffer);

 // Resolve extraction model
 const [provider, ...modelParts] = CONFIG.extractModel.split("/");
 const modelId = modelParts.join("/"); // handle model IDs with slashes

 let model = ctx.modelRegistry.find(provider, modelId);
 if (!model) {
  // Fall back to session's active model
  ctx.ui.notify(
   `Alexandria extraction: model ${CONFIG.extractModel} not available, falling back to session model.`,
   "warning",
  );
  model = ctx.model;
  if (!model) return [];
 }

 // Call model with timeout
 const controller = new AbortController();
 const timeout = setTimeout(() => controller.abort(), CONFIG.extractTimeoutMs);

 try {
  const response = await ctx.modelRegistry.complete(model, {
   messages: [{ role: "user" as const, content: userMessage }],
  });

  clearTimeout(timeout);

  // Extract text from response
  const responseText = typeof response.content === "string"
   ? response.content
   : Array.isArray(response.content)
    ? response.content
     .filter((b: any) => b?.type === "text")
     .map((b: any) => b.text)
     .join("")
    : "";

  if (!responseText) return [];

  // Parse JSON from response — handle markdown code fences
  const jsonText = responseText.replace(/^```(?:json)?\s*\n?/m, "").replace(/\n?```\s*$/m, "").trim();
  const parsed = JSON.parse(jsonText) as ExtractionResult;

  if (!Array.isArray(parsed.memories)) return [];

  return parsed.memories
   .filter(m => typeof m.content === "string" && m.content.length > 0)
   .map(m => ({
    content: m.content,
    tags: Array.isArray(m.tags) ? m.tags.filter(t => typeof t === "string") : ["extracted"],
   }));
 } catch (err) {
  clearTimeout(timeout);
  if (controller.signal.aborted) {
   ctx.ui.notify("Alexandria extraction timed out; skipping.", "warning");
  }
  // Fail open — don't log the full error to avoid noise
  return [];
 }
}
```

**Step 2: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/extraction.ts
git commit -m "feat(ext): add LLM extraction module for session_shutdown"
```

---

### Task 9: Wire Everything Into index.ts

**Files:**

- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/index.ts`

**Step 1: Rewrite `index.ts` as the wiring layer**

This is the main integration step. Wire all hooks to their modules:

```typescript
// src/index.ts
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { CONFIG } from "./config.js";
import { resetClient, closeClient, storeMemory } from "./mcp-client.js";
import { retrieveMemories, formatMemoriesBlock } from "./recall.js";
import { SessionDedupBuffer } from "./detectors/types.js";
import { detectCorrection } from "./detectors/correction.js";
import { detectPreference } from "./detectors/preference.js";
import { trackToolStore } from "./detectors/tool-tracker.js";
import { ErrorTracker } from "./detectors/error-tracker.js";
import { runExtraction } from "./extraction.js";

export default function alexandriaExtension(pi: ExtensionAPI) {
 // Session-scoped state — reset on each session
 let dedupBuffer = new SessionDedupBuffer();
 let errorTracker = new ErrorTracker();

 // ── Recall (existing behavior) ──────────────────────────────────────
 if (!CONFIG.recallDisabled) {
  pi.on("before_agent_start", async (event, ctx) => {
   const query = event.prompt?.trim();
   if (!query) return;

   try {
    const memories = await retrieveMemories(query);
    if (memories.length === 0) return;

    return {
     message: {
      customType: "alexandria-auto-recall",
      content: formatMemoriesBlock(memories),
      display: true,
     },
    };
   } catch (err) {
    resetClient();
    ctx.ui.notify(
     `Alexandria auto-recall failed (${err instanceof Error ? err.message : String(err)}); continuing without it.`,
     "warning",
    );
    return;
   }
  });
 }

 // ── Store: Heuristic detectors ──────────────────────────────────────
 if (!CONFIG.storeDisabled) {
  // Correction + preference detection on user prompts
  pi.on("before_agent_start", async (event, ctx) => {
   const prompt = event.prompt?.trim();
   if (!prompt) return;

   const detections = [
    detectCorrection(prompt, dedupBuffer),
    detectPreference(prompt, dedupBuffer),
   ].filter((d): d is NonNullable<typeof d> => d !== null);

   // Fire-and-forget stores — don't block the agent turn
   for (const detection of detections) {
    storeMemory(detection.content, detection.tags).catch(err => {
     resetClient();
     // Silent fail — heuristic stores are best-effort
    });
   }
  });

  // Tool dedup tracker — watch for agent-initiated store_memory calls
  pi.on("tool_result", async (event) => {
   await trackToolStore(
    { toolName: event.toolName ?? "", input: event.input ?? {}, isError: event.isError },
    dedupBuffer,
   ).catch(() => {}); // never fail on tracking
  });

  // Error resolution tracker — accumulate errors and successes
  pi.on("tool_execution_end", async (event) => {
   if (event.isError) {
    errorTracker.recordError(event.toolName, event.result);
   } else {
    errorTracker.recordSuccess(event.toolName, event.result);
   }
  });

  // Flush error resolutions at agent_end
  pi.on("agent_end", async () => {
   const resolutions = errorTracker.flush();
   for (const mem of resolutions) {
    storeMemory(mem.content, mem.tags).catch(() => {});
   }
  });
 }

 // ── Session shutdown ────────────────────────────────────────────────
 pi.on("session_shutdown", async (event, ctx) => {
  // LLM extraction — skip on reload (no meaningful conversation boundary)
  if (!CONFIG.storeDisabled && event.reason !== "reload") {
   try {
    const extracted = await runExtraction(ctx as any, dedupBuffer);
    for (const mem of extracted) {
     await storeMemory(mem.content, [...mem.tags, "extracted"]).catch(() => {});
    }
   } catch (err) {
    ctx.ui.notify(
     `Alexandria extraction failed (${err instanceof Error ? err.message : String(err)}); skipping.`,
     "warning",
    );
   }
  }

  // Reset session state
  dedupBuffer = new SessionDedupBuffer();
  errorTracker = new ErrorTracker();

  // Close MCP client
  await closeClient();
 });

 // Reset state on session_start (handles /new, /resume, /fork)
 pi.on("session_start", async () => {
  dedupBuffer = new SessionDedupBuffer();
  errorTracker = new ErrorTracker();
 });
}
```

**Step 2: Verify extension loads**

Run: `pi -e contrib/pi/extensions/alexandria-auto-recall/src/index.ts`
Expected: Extension loads without errors. Test a basic prompt to verify recall still works.

**Step 3: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/index.ts
git commit -m "feat(ext): wire heuristic detectors + LLM extraction into extension"
```

---

### Task 10: Update Extension Header Comment and Package Version

**Files:**

- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/index.ts` (header comment)
- Modify: `contrib/pi/extensions/alexandria-auto-recall/package.json` (version bump)

**Step 1: Update the file-level JSDoc in `index.ts`**

Replace the existing header comment with an updated one that documents both recall and
store behavior, all config env vars.

**Step 2: Bump `package.json` version to `2.0.0`**

This is a significant feature addition — breaking if someone relied on the extension
being recall-only (though it's private/internal, so semver is informational).

**Step 3: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/
git commit -m "docs(ext): update header docs and bump to v2.0.0"
```

---

### Task 11: Manual Integration Test

No automated test framework for pi extensions — this is a manual verification step.

**Step 1: Start Alexandria server**

```bash
alexandria &
```

Expected: Server starts on `http://127.0.0.1:3000/mcp`.

**Step 2: Start pi with the extension**

```bash
pi -e contrib/pi/extensions/alexandria-auto-recall/src/index.ts
```

**Step 3: Test correction detection**

Type: `no, always use rg instead of grep`
Expected: Extension silently stores a correction memory. Verify with:

```
/mcp alexandria retrieve_memories {"query": "rg grep", "limit": 5}
```

**Step 4: Test preference detection**

Type: `I prefer just over Makefiles for task runners`
Expected: Extension stores a preference memory.

**Step 5: Test tool dedup**

Ask the agent something that triggers a skill-driven `store_memory` call (e.g., "remember that this project uses SurrealDB 3.2"). Verify the tool tracker recorded it.

**Step 6: Test extraction at shutdown**

Have a short conversation with a decision or architectural choice. Exit pi with Ctrl+D.
Expected: The extraction model runs at shutdown and stores any remaining durable facts.
Verify by restarting pi and checking auto-recall.

**Step 7: Test `ALEXANDRIA_AUTO_STORE=off`**

```bash
ALEXANDRIA_AUTO_STORE=off pi -e contrib/pi/extensions/alexandria-auto-recall/src/index.ts
```

Make a correction. Verify no memory is stored.

**Step 8: Final commit**

If all manual tests pass:

```bash
git add -A
git commit -m "feat(ext): auto-store extension — heuristic detectors + LLM extraction

Implements the auto-store design from docs/plans/2026-08-18-auto-store-extension-design.md.

Three-layer write funnel:
- Skill (existing): agent decides during conversation
- Heuristic detectors (new): corrections, preferences, error resolutions
- LLM extraction (new): session_shutdown sweep via configurable model

Config: ALEXANDRIA_AUTO_STORE, ALEXANDRIA_EXTRACT_MODEL, ALEXANDRIA_EXTRACT_TIMEOUT_MS

Implemented with the help of Claude Code"
```

---

Plan complete and saved to `docs/plans/2026-08-18-auto-store-implementation.md`. Two execution options:

**1. Subagent-Driven (this session)** - I dispatch fresh subagent per task, review between tasks, fast iteration

**2. Parallel Session (separate)** - Open new session with executing-plans, batch execution with checkpoints

Which approach?
