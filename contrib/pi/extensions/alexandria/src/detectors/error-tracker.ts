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
}

interface Resolution {
	error: ErrorRecord;
	successText: string;
}

const MAX_ERRORS = 5;
const MAX_TEXT_LENGTH = 200;
const MIN_ERROR_LENGTH = 30;

/**
 * Errors that are not durable lessons: the tool rejected our own call shape, or
 * told us the edit drifted and to retry. Derived from the live store rather than
 * invented — 526 `error-resolution` rows collapsed into 396 classes, 351 of them
 * singletons, and the head of that distribution is entirely these families
 * (`edit` PARTIAL APPLY ×23, `Validation failed for tool` ×22 across ask_user /
 * ask_user_question / ctx_execute, `Path not found` ×9, `fatal: not a git
 * repository` ×7, `Failed to call tool: Missing` ×14).
 *
 * An allowlist, not a denylist of "interesting" errors, because the two failure
 * directions are not symmetric: a false entry here loses one memory, while a
 * false entry the other way re-poisons every future recall — these rows are
 * retrieved again and again, and 13% of the results on a set of realistic dev
 * probes already were error-resolution rows.
 *
 * The marker words are pi's own (RETRYABLE / PARTIAL APPLY / Edit without read),
 * so this tracks pi's tool protocol rather than a guess at what is transient.
 */
const TRANSIENT_PATTERNS: RegExp[] = [
	/RETRYABLE/,
	/PARTIAL APPLY/,
	/Edit without read/,
	/Validation failed for tool/,
	/failed to deserialize parameters/i,
	/Failed to call tool:/,
	/^\s*(Error: )?Path not found:/,
	/fatal: not a git repository/,
];

/**
 * Elide the payload a tool quoted back at us — the `oldText ("…")` of a drifted
 * edit, an inline code span, a long literal — so that one error class reduces to
 * one row. Without this, every occurrence differs by its quoted content, which is
 * what turned 396 classes out of 526 rows into mostly singletons and let the same
 * lesson be stored a dozen times.
 */
function normalizeErrorText(text: string): string {
	return text
		.replace(/\(([^)]{16,})\)/g, "(<elided>)")
		.replace(/`([^`]{8,})`/g, "`<elided>`")
		.replace(/"([^"]{24,})"/g, '"<elided>"')
		.replace(/\s+/g, " ")
		.trim();
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
		const errorText = normalizeErrorText(text.slice(0, MAX_TEXT_LENGTH));

		// Filter: too short to be meaningful
		if (errorText.length < MIN_ERROR_LENGTH) return;

		// Filter: must contain error-indicative language
		if (!ERROR_SIGNAL_PATTERN.test(errorText)) return;

		// Filter: transient tool protocol, not a lesson
		if (TRANSIENT_PATTERNS.some((re) => re.test(errorText))) return;

		// Filter: this class is already queued or already stored this session
		const key = `${toolName}\u0000${errorText}`;
		if (this.seen.has(key)) return;
		this.seen.add(key);

		// Ring buffer — drop oldest if full
		if (this.errors.length >= MAX_ERRORS) {
			this.errors.shift();
		}

		this.errors.push({ toolName, errorText, timestamp: Date.now() });
	}

	/**
	 * Record a tool success. Caller should pass pre-extracted text
	 * (not the raw MCP response blob).
	 */
	recordSuccess(toolName: string, text: string): void {
		const successText = normalizeErrorText(text.slice(0, MAX_TEXT_LENGTH));
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

		this.resolutions = [];
		this.errors = [];
		return memories;
	}
}
