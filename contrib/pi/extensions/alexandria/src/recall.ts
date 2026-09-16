/**
 * Auto-recall: queries Alexandria for memories relevant to the user's prompt.
 */

import {
	callToolWithRetry,
	extractTextContent,
	PROMPT_CALL_TIMEOUT_MS,
	type CallTool,
} from "./mcp-client.js";
import { CONFIG } from "./config.js";

export interface RetrievedMemory {
	id: string;
	content: string;
	similarity: number;
	tags?: string[];
}

interface RetrieveMemoriesResponse {
	results?: RetrievedMemory[];
	error?: string;
}

/** What one recall probe produced: usable memories, plus a payload-level failure
 *  worth telling the user about. `error` is never set at the same time as a
 *  non-empty `items`. */
export interface RecallCheck {
	items: RetrievedMemory[];
	error?: string;
}

export async function retrieveMemories(
	query: string,
	call: CallTool = callToolWithRetry,
	signal?: AbortSignal,
): Promise<RecallCheck> {
	// Bounded: this awaits before the turn starts, so a hung server must not be
	// able to spend a minute of it. A timeout costs one prompt of recall, not a
	// blocked turn.
	const result = await call(
		"retrieve_memories",
		{ query, limit: CONFIG.recallLimit },
		PROMPT_CALL_TIMEOUT_MS,
		signal,
	);

	// A payload problem is not a transport problem: the reply came back, it just
	// carried nothing usable. Reporting it through `error` (like the reminder path)
	// keeps the shared client alive and makes "the server is broken" distinguishable
	// from "nothing matched", which a bare `[]` conflated.
	if (result.isError === true) {
		return {
			items: [],
			error:
				extractTextContent(result.content) ??
				"retrieve_memories reported an error",
		};
	}
	const text = extractTextContent(result.content);
	if (!text) {
		return { items: [], error: "no text content in retrieve_memories response" };
	}

	let parsed: RetrieveMemoriesResponse;
	try {
		parsed = JSON.parse(text) as RetrieveMemoriesResponse;
	} catch {
		return { items: [], error: "malformed retrieve_memories response" };
	}

	// The payload came from JSON, so its shape is not the type's promise: a null
	// body, a non-array `results`, or null entries must not throw out of here —
	// that would read to the user as "auto-recall failed" plus a needless client
	// reset for what is really an unusable response.
	if (!Array.isArray(parsed?.results)) {
		return {
			items: [],
			error: "no results list in retrieve_memories response",
		};
	}
	const results = parsed.results.filter(
		(m): m is RetrievedMemory =>
			!!m && typeof m === "object" && typeof m.content === "string",
	);
	return {
		items: results.filter((m) => Number(m.similarity) >= CONFIG.recallMinSimilarity),
	};
}

/** Collapse newlines so one memory stays one bullet. Shared shape with
 *  `formatDueBlock`: both blocks are joined into a single injected message, and a
 *  multi-line entry reads as a continuation of the item above it. */
const oneLine = (s: unknown) =>
	String(s)
		.trim()
		.replace(/\s*\n\s*/g, " ");

export function formatMemoriesBlock(memories: RetrievedMemory[]): string {
	const lines = memories.map((m) => {
		const tags =
			Array.isArray(m.tags) && m.tags.length > 0
				? ` [${m.tags.map((t) => oneLine(t)).join(", ")}]`
				: "";
		// `id` is what `update_memory` and `delete_memory` need, and the payload is
		// JSON, so it can be missing: render a marker rather than the word
		// "undefined" into the context the model is asked to trust.
		const id = m.id ? oneLine(m.id) : "?";
		// `similarity` is likewise unvalidated; `Number()` keeps a bad value from
		// throwing inside a formatter whose throw would read as "recall failed".
		const similarity = Number(m.similarity);
		const score = Number.isFinite(similarity)
			? similarity.toFixed(2)
			: "n/a";
		return `- (similarity ${score}, id ${id})${tags} ${oneLine(m.content)}`;
	});
	return [
		"Relevant memories retrieved automatically from Alexandria for this prompt:",
		...lines,
		"",
		"These are surfaced proactively; verify relevance before relying on them, and use update_memory if any is stale.",
	].join("\n");
}
