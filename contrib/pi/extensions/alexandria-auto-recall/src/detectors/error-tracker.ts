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

export class ErrorTracker {
	private errors: ErrorRecord[] = [];
	private resolutions: Resolution[] = [];

	recordError(toolName: string, result: unknown): void {
		const errorText = this.summarize(result);
		if (!errorText) return;

		// Ring buffer — drop oldest if full
		if (this.errors.length >= MAX_ERRORS) {
			this.errors.shift();
		}

		this.errors.push({ toolName, errorText, timestamp: Date.now() });
	}

	recordSuccess(toolName: string, result: unknown): void {
		// Find a matching error for this tool
		const errorIdx = this.errors.findIndex((e) => e.toolName === toolName);
		if (errorIdx === -1) return;

		const error = this.errors[errorIdx];
		this.errors.splice(errorIdx, 1);

		const successText = this.summarize(result);
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

	private summarize(result: unknown): string | null {
		if (typeof result === "string") {
			return result.slice(0, 300);
		}
		if (result && typeof result === "object") {
			const r = result as Record<string, unknown>;
			const text = r.text ?? r.message ?? r.error;
			if (typeof text === "string") return text.slice(0, 300);
			try {
				return JSON.stringify(result).slice(0, 300);
			} catch {
				return null;
			}
		}
		return null;
	}
}
