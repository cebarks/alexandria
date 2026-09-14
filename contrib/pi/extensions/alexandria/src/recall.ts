/**
 * Auto-recall: queries Alexandria for memories relevant to the user's prompt.
 */

import {
	callToolWithRetry,
	extractTextContent,
	PROMPT_CALL_TIMEOUT_MS,
} from "./mcp-client.js";
import { CONFIG } from "./config.js";

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

export async function retrieveMemories(
	query: string,
): Promise<RetrievedMemory[]> {
	// Bounded: this awaits before the turn starts, so a hung server must not be
	// able to spend a minute of it. A timeout costs one prompt of recall, not a
	// blocked turn.
	const result = await callToolWithRetry(
		"retrieve_memories",
		{ query, limit: CONFIG.recallLimit },
		PROMPT_CALL_TIMEOUT_MS,
	);

	const text = extractTextContent(result.content);
	if (!text) return [];

	let parsed: RetrieveMemoriesResponse;
	try {
		parsed = JSON.parse(text) as RetrieveMemoriesResponse;
	} catch {
		return [];
	}

	// The payload came from JSON, so its shape is not the type's promise: a null
	// body, a non-array `results`, or null entries must not throw out of here —
	// that would read to the user as "auto-recall failed" plus a needless client
	// reset for what is really an unusable response.
	const results = Array.isArray(parsed?.results)
		? parsed.results.filter(
				(m): m is RetrievedMemory =>
					!!m && typeof m === "object" && typeof m.content === "string",
			)
		: [];
	return results.filter((m) => m.similarity >= CONFIG.recallMinSimilarity);
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
