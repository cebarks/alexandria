/**
 * Preference detector — scans user prompts for forward-looking preference/convention statements.
 */

import type { SessionDedupBuffer, DetectedMemory } from "./types.js";

// Every pattern captures from its trigger onward ("never commit Cargo.lock", not "commit
// Cargo.lock"): a capture that starts after the trigger drops the polarity, and an inverted
// preference is worse than a missing one because it is recalled as "User preference:".
// Negative triggers come first so "don't always rebase" is stored whole. A bare don't / do not
// only counts at the start of a clause or after "please": mid-clause it is a statement about
// the speaker ("I don't know why this fails"). The two lead-ins at the end carry no polarity,
// so what follows them is the statement.
const PREFERENCE_PATTERNS: RegExp[] = [
	/(?:^|[.!?;:,]\s*|\bplease\s+)((?:don'?t|do\s+not)\s+.+)/i,
	/\b(don'?t\s+ever\s+.+)/i,
	/\b(never\s+.+)/i,
	/\b(always\s+.+)/i,
	/\b(i\s+prefer\s+.+)/i,
	/\b(i\s+like\s+.+?\s+better\b.*)/i,
	/\b(default\s+to\s+.+)/i,
	/\b(make\s+sure\s+to\s+.+)/i,
	/\b(use\s+.+?\s+instead\s+of\s+.+)/i,
	/\bfrom\s+now\s+on[,.]?\s+(.+)/i,
	/\bgoing\s+forward[,.]?\s+(.+)/i,
];

// A negation earlier in the same clause governs the match ("it's not that I prefer tabs"), and no
// capture here can keep it, so such a match is never stored. A "no," lead-in is its own clause.
const NEGATION = /\b(?:not|no|never|cannot|without)\b|n'?t\b/i;

export function detectPreference(
	prompt: string,
	buffer: SessionDedupBuffer,
): DetectedMemory | null {
	const trimmed = prompt.trim();
	if (trimmed.length < 8 || trimmed.length > 500) return null;

	for (const pattern of PREFERENCE_PATTERNS) {
		const match = trimmed.match(pattern);
		if (match?.[1]) {
			const captureStart = (match.index ?? 0) + match[0].length - match[1].length;
			const clauseSoFar = trimmed.slice(0, captureStart).split(/[.!?;:,]/).pop() ?? "";
			if (NEGATION.test(clauseSoFar)) continue;

			const statement = match[1].replace(/[.!]+$/, "").trim();
			if (statement.length < 5) continue;

			const content = `User preference: ${statement}`;

			if (!buffer.addHeuristicStore(content)) return null;

			return {
				content,
				tags: ["preference", "auto-detected", "source:regex"],
			};
		}
	}

	return null;
}
