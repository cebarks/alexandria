/**
 * LLM extraction pass — runs at session_shutdown to extract durable facts
 * from the conversation that heuristics and the skill missed.
 *
 * Uses ctx.modelRegistry.find() + ctx.modelRegistry.complete() to route through
 * pi's model infrastructure — handles Vertex OAuth, Anthropic API keys, etc.
 * without any provider-specific HTTP code.
 */

import { CONFIG } from "./config.js";
import type { SessionDedupBuffer } from "./detectors/types.js";
import {
	type ExtractionResult,
	extractText,
	parseExtractionResponse,
	serializeEntries,
} from "./extraction-parse.js";

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

Also summarize the session itself: one or two sentences on what was worked on and what the outcome was, plus a few lowercase tags for the session as a whole.

Respond with JSON only:
{
  "memories": [
    {"content": "standalone statement", "tags": ["relevant", "tags"]},
    ...
  ],
  "summary": "what the session accomplished",
  "tags": ["session", "tags"]
}

If nothing is worth extracting, leave "memories" empty but still fill in "summary" and "tags".`;

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
): Promise<ExtractionResult> {
	const entries = ctx.sessionManager.buildContextEntries();
	const serialized = serializeEntries(entries);

	// Skip extraction if conversation is trivially short
	if (serialized.length < 100) return { memories: [] };

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
		if (!model) return { memories: [] };
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

		return parseExtractionResponse(extractText(response.content));
	} catch (err) {
		if (err instanceof Error && err.message === "extraction_timeout") {
			ctx.ui.notify("Alexandria extraction timed out; skipping.", "warning");
		}
		// Fail open
		return { memories: [] };
	}
}
