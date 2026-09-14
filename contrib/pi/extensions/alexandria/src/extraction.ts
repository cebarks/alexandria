/**
 * LLM extraction pass — runs at session_shutdown to extract durable facts
 * from the conversation that heuristics and the skill missed.
 *
 * Uses ctx.modelRegistry.find() + ctx.modelRegistry.complete() to route through
 * pi's model infrastructure — handles Vertex OAuth, Anthropic API keys, etc.
 * without any provider-specific HTTP code.
 */

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
				const text = extractText(content);
				if (text) lines.push(`[Turn ${turnNum} - User]: ${text}`);
			} else if (role === "assistant") {
				const text = extractText(content);
				if (text) lines.push(`[Turn ${turnNum} - Assistant]: ${text}`);
			}
		} else if (e.type === "compaction") {
			const summary =
				(e as Record<string, unknown>).summary ??
				(
					(e as Record<string, unknown>).compaction as
						| Record<string, unknown>
						| undefined
				)?.summary;
			if (typeof summary === "string") {
				lines.push(`[Session Summary]: ${summary}`);
			}
		}
	}

	return lines.join("\n\n");
}

/** Extract plain text from a message content field (string or content blocks). */
function extractText(content: unknown): string {
	if (typeof content === "string") return content;
	if (Array.isArray(content)) {
		return content
			.filter((b: Record<string, unknown>) => b?.type === "text")
			.map((b: Record<string, unknown>) => b.text as string)
			.join("\n");
	}
	return "";
}

/**
 * Build the full extraction prompt with conversation and "already stored" context.
 */
function buildPrompt(
	serializedConversation: string,
	buffer: SessionDedupBuffer,
): string {
	const alreadyStored = buffer.getAllStoredContents();
	const alreadyStoredBlock =
		alreadyStored.length > 0
			? alreadyStored.map((c) => `- ${c}`).join("\n")
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

/** Minimal ctx shape — avoids importing full pi types as a runtime dependency. */
interface ExtractionContext {
	sessionManager: { buildContextEntries(): unknown[] };
	modelRegistry: {
		find(provider: string, modelId: string): unknown | undefined;
		complete(
			model: unknown,
			context: { messages: Array<{ role: string; content: string }> },
		): Promise<unknown>;
	};
	model: unknown;
	ui: { notify(msg: string, level: string): void };
}

/**
 * Run the LLM extraction pass. Falls back to ctx.model if the configured
 * extraction model/provider is not available.
 */
export async function runExtraction(
	ctx: ExtractionContext,
	buffer: SessionDedupBuffer,
): Promise<DetectedMemory[]> {
	const entries = ctx.sessionManager.buildContextEntries();
	const serialized = serializeEntries(entries);

	// Skip extraction if conversation is trivially short
	if (serialized.length < 100) return [];

	// Cap serialized conversation at ~16k tokens (~64k chars)
	const maxChars = 64_000;
	const truncated =
		serialized.length > maxChars
			? serialized.slice(serialized.length - maxChars)
			: serialized;

	const userMessage = buildPrompt(truncated, buffer);

	// Resolve extraction model
	const [provider, ...modelParts] = CONFIG.extractModel.split("/");
	const modelId = modelParts.join("/"); // handle model IDs with slashes

	let model = ctx.modelRegistry.find(provider, modelId);
	if (!model) {
		ctx.ui.notify(
			`Alexandria extraction: model ${CONFIG.extractModel} not available, falling back to session model.`,
			"warning",
		);
		model = ctx.model;
		if (!model) return [];
	}

	// Call model with timeout via Promise.race — ctx.modelRegistry.complete()
	// may not support AbortSignal, so we race against a rejection timer.
	const timeoutPromise = new Promise<never>((_, reject) => {
		setTimeout(
			() => reject(new Error("extraction_timeout")),
			CONFIG.extractTimeoutMs,
		);
	});

	try {
		const response = (await Promise.race([
			ctx.modelRegistry.complete(model, {
				messages: [{ role: "user", content: userMessage }],
			}),
			timeoutPromise,
		])) as Record<string, unknown>;

		// Extract text from response
		const responseText = extractText(response.content);
		if (!responseText) return [];

		// Parse JSON from response — handle markdown code fences
		const jsonText = responseText
			.replace(/^```(?:json)?\s*\n?/m, "")
			.replace(/\n?```\s*$/m, "")
			.trim();
		const parsed = JSON.parse(jsonText) as ExtractionResult;

		if (!Array.isArray(parsed.memories)) return [];

		return parsed.memories
			.filter((m) => typeof m.content === "string" && m.content.length > 0)
			.map((m) => ({
				content: m.content,
				tags: Array.isArray(m.tags)
					? m.tags.filter((t) => typeof t === "string")
					: ["extracted"],
			}));
	} catch (err) {
		if (err instanceof Error && err.message === "extraction_timeout") {
			ctx.ui.notify("Alexandria extraction timed out; skipping.", "warning");
		}
		// Fail open
		return [];
	}
}
