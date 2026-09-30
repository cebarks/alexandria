/**
 * Error resolution tracker — pairs tool errors with subsequent successes.
 *
 * Call recordError() on tool_execution_end when isError=true.
 * Call recordSuccess() on tool_execution_end when isError=false.
 * Call flush() at agent_end to emit paired resolutions.
 *
 * Not every error is a lesson. recordError() drops the transient tool-protocol
 * classes (see TRANSIENT_PATTERNS), elides the payload a tool quoted back so one
 * class yields one row, and never stores a class twice in a session — without
 * those filters this path wrote 526 rows, 36.5% of the live store, mostly
 * restatements of "the tool already told you to retry".
 */

import type { DetectedMemory } from "./types.js";

interface ErrorRecord {
	toolName: string;
	errorText: string;
	timestamp: number;
	/** Class key held in `seen`, so eviction can release it. */
	key: string;
}

interface Resolution {
	error: ErrorRecord;
	successText: string;
}

const MAX_ERRORS = 5;
const MAX_TEXT_LENGTH = 200;
const MIN_ERROR_LENGTH = 30;

/**
 * Errors that are not durable lessons.
 *
 * Two of these marker families are pi-lens's (4.3.0 emits `🔄 RETRYABLE — Edit
 * target not found`, `⚠️ PARTIAL APPLY — N edits committed …`); they are NOT pi's
 * own wording, which is why each requires the em dash: the dash is what separates
 * "this is pi-lens telling me to retry" from a durable error that merely mentions
 * the word. Consequence of the dependency: on a host without pi-lens these two
 * rules never fire, and a pi-lens reword silently degrades the gate — nothing in
 * the suite would notice. TODO(debt): pin the markers to pi-lens's emitted strings
 * rather than prose, or gate on the tool's structured error code.
 *
 * The rest are measured from the live store: `Validation failed for tool` ×85,
 * `edit PARTIAL APPLY` ×47, pi `RETRYABLE` ×44, gateway misuse ×26.
 */
const TRANSIENT_PATTERNS: RegExp[] = [
	/RETRYABLE —/,
	/PARTIAL APPLY —/,
	/Edit without read/,
	/(^|: )Path not found:/,
	/fatal: not a git repository/,
	/Failed to call tool: Missing/,
];

/**
 * Call-shape rejections: usually noise about how the agent addressed some other
 * server's tool. NOT applied to the alexandria server's own tools — a rejection
 * from `mcp__alexandria__store_memory` is a constraint of THIS product's API, and
 * it is exactly the "error -> what worked" lesson this module exists to write.
 * Filtering it fleet-wide would keep deleting the evidence for claims like the
 * stored (and still unverified) one that store_memory rejects array tags.
 */
const CALL_SHAPE_PATTERNS: RegExp[] = [
	/Validation failed for tool/,
	/failed to deserialize parameters/i,
];

function isTransient(toolName: string, errorText: string): boolean {
	if (TRANSIENT_PATTERNS.some((re) => re.test(errorText))) return true;
	if (toolName.includes("alexandria")) return false;
	return CALL_SHAPE_PATTERNS.some((re) => re.test(errorText));
}

/**
 * Elide the payload a tool echoed back inside parentheses — the `oldText ("…")` /
 * `(edits[0], edits[1])` runs of a drifted edit — so one class yields one row.
 *
 * Deliberately narrow, because the first version of this was too wide and the
 * review caught it: eliding backticks and quoted runs destroyed the
 * discriminator (``cannot find function `parse_tags` `` and ``…`store_batch```
 * collapsed into one key) and gutted the resolution half of the memory, which is
 * the entire point of the row. Whitespace is still collapsed, since that only
 * stabilises the class key.
 */
function normalizeErrorText(text: string): string {
	return text.replace(/\(([^)]{16,})\)/g, "(<elided>)").replace(/\s+/g, " ").trim();
}

/** Error text must contain at least one of these to be worth tracking. */
const ERROR_SIGNAL_PATTERN =
	/\b(error|fail(ed|ure)?|exception|panic|denied|not found|timeout|refused|abort|crash|fatal|invalid|cannot|couldn'?t|unable|unexpected|broken|missing|violation)\b/i;

export class ErrorTracker {
	private errors: ErrorRecord[] = [];
	private resolutions: Resolution[] = [];
	/** Normalized class keys already tracked this session, so a class is stored once. */
	private seen = new Set<string>();

	/**
	 * Record a tool error. Caller should pass pre-extracted text
	 * (not the raw MCP response blob).
	 */
	recordError(toolName: string, text: string): void {
		const raw = text.slice(0, MAX_TEXT_LENGTH).trim();

		// Length and signal are judged on the untouched text: elision normalises the
		// class key and the stored row, and must never be able to shrink an error out
		// of existence.
		if (raw.length < MIN_ERROR_LENGTH) return;
		if (!ERROR_SIGNAL_PATTERN.test(raw)) return;

		const errorText = normalizeErrorText(raw);

		// Filter: transient tool protocol, not a lesson
		if (isTransient(toolName, errorText)) return;

		// Filter: this class is already queued or already stored this session
		const key = `${toolName}\u0000${errorText}`;
		if (this.seen.has(key)) return;
		this.seen.add(key);

		// Ring buffer — drop oldest if full, and release its claim: an error that
		// left the buffer unpaired taught us nothing, so the class must be able to
		// queue again and still pair with a later success.
		if (this.errors.length >= MAX_ERRORS) {
			const evicted = this.errors.shift();
			if (evicted) this.seen.delete(evicted.key);
		}

		this.errors.push({ toolName, errorText, timestamp: Date.now(), key });
	}

	/**
	 * Record a tool success. Caller should pass pre-extracted text
	 * (not the raw MCP response blob).
	 */
	recordSuccess(toolName: string, text: string): void {
		// Not normalised: the resolution is the half of the memory that says what
		// fixed it, and eliding it makes the row useless in a different way.
		const successText = text.slice(0, MAX_TEXT_LENGTH).trim();
		if (!successText) return;

		// Find a matching error for this tool
		const errorIdx = this.errors.findIndex((e) => e.toolName === toolName);
		if (errorIdx === -1) return;

		const error = this.errors[errorIdx];
		this.errors.splice(errorIdx, 1);

		this.resolutions.push({ error, successText });
	}

	/** Flush all paired resolutions as DetectedMemory[]. Clears internal state. */
	flush(): DetectedMemory[] {
		const memories = this.resolutions.map((r) => ({
			content: `Error with ${r.error.toolName}: ${r.error.errorText}\nResolution: ${r.successText}`,
			tags: ["error-resolution", "auto-detected", r.error.toolName],
		}));

		// Errors still unpaired when agent_end arrives have taught us nothing, so
		// release their class claims — the same error recurring next turn, followed by
		// a real fix, is still a lesson worth storing.
		for (const e of this.errors) this.seen.delete(e.key);

		this.resolutions = [];
		this.errors = [];
		return memories;
	}
}
