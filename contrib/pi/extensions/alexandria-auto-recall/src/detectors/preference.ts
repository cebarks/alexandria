/**
 * Preference detector — scans user prompts for forward-looking preference/convention statements.
 */

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
