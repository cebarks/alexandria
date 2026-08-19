/**
 * Correction detector — scans user prompts for correction-shaped language.
 * Fires when unambiguous; defers ambiguous cases to the LLM extraction pass.
 */

import type { SessionDedupBuffer, DetectedMemory } from "./types.js";

/**
 * Patterns that indicate the user is correcting the agent.
 * Each pattern captures the corrected statement in group 1.
 * Order matters: more specific patterns first to avoid greedy matches.
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
