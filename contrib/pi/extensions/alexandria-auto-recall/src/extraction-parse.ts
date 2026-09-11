/**
 * Pure helpers for the extraction pass: conversation serialization and
 * response parsing. Kept dependency-free so they can be unit tested without
 * loading config or the model registry.
 */

import type { DetectedMemory } from "./detectors/types.js";

/**
 * Serialize session entries into a text representation for the extraction prompt.
 * Strips tool call details, keeps user/assistant text + compaction summaries.
 */
export function serializeEntries(entries: unknown[]): string {
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
				const text = extractText(content);
				if (text) {
					turnNum++;
					lines.push(`[Turn ${turnNum} - User]: ${text}`);
				}
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
export function extractText(content: unknown): string {
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
 * Parse the model's response text into memories. Tolerates markdown code
 * fences; anything unparseable or wrongly shaped yields [] (fail open).
 */
export function parseExtractionResponse(responseText: string): DetectedMemory[] {
	if (!responseText) return [];

	const jsonText = responseText
		.replace(/^```(?:json)?\s*\n?/m, "")
		.replace(/\n?```\s*$/m, "")
		.trim();

	let parsed: { memories?: unknown };
	try {
		parsed = JSON.parse(jsonText);
	} catch {
		return [];
	}

	if (!Array.isArray(parsed?.memories)) return [];

	return parsed.memories
		.filter((m) => typeof m?.content === "string" && m.content.length > 0)
		.map((m) => ({
			content: m.content as string,
			tags: Array.isArray(m.tags)
				? m.tags.filter((t: unknown): t is string => typeof t === "string")
				: ["extracted"],
		}));
}
