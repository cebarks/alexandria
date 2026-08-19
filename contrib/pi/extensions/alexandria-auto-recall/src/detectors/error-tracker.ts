/**
 * Error resolution tracker — pairs tool errors with subsequent successes.
 *
 * Call recordError() on tool_execution_end when isError=true.
 * Call recordSuccess() on tool_execution_end when isError=false.
 * Call flush() at agent_end to emit paired resolutions.
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

/** Error text must contain at least one of these to be worth tracking. */
const ERROR_SIGNAL_PATTERN =
	/\b(error|fail(ed|ure)?|exception|panic|denied|not found|timeout|refused|abort|crash|fatal|invalid|cannot|couldn'?t|unable|unexpected|broken|missing|violation)\b/i;

export class ErrorTracker {
	private errors: ErrorRecord[] = [];
	private resolutions: Resolution[] = [];

	/**
	 * Record a tool error. Caller should pass pre-extracted text
	 * (not the raw MCP response blob).
	 */
	recordError(toolName: string, text: string): void {
		const errorText = text.slice(0, MAX_TEXT_LENGTH).trim();

		// Filter: too short to be meaningful
		if (errorText.length < MIN_ERROR_LENGTH) return;

		// Filter: must contain error-indicative language
		if (!ERROR_SIGNAL_PATTERN.test(errorText)) return;

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
		// Find a matching error for this tool
		const errorIdx = this.errors.findIndex((e) => e.toolName === toolName);
		if (errorIdx === -1) return;

		const error = this.errors[errorIdx];
		this.errors.splice(errorIdx, 1);

		const successText = text.slice(0, MAX_TEXT_LENGTH).trim();
		if (!successText) return;

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
