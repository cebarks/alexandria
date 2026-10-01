/**
 * Error resolution tracker — pairs tool errors with subsequent successes.
 *
 * Call recordError() on tool_execution_end when isError=true.
 * Call recordSuccess() on tool_execution_end when isError=false.
 * Call flush() at agent_end to emit paired resolutions.
 *
 * Not every error is a lesson. recordError() drops the transient tool-protocol
 * classes (see TRANSIENT_PATTERNS / CALL_SHAPE_PATTERNS / isTransient), collapses
 * whitespace so one class yields one key, and never stores a class twice in a
 * session.
 * Without those filters this path was the single largest writer in the store; the
 * measurements and their caveat live once, in TODO-misc.md under "ErrorTracker's
 * transient gate", rather than being copied into this comment.
 */

import type { DetectedMemory } from "./types.js";

interface ErrorRecord {
	toolName: string;
	errorText: string;
	timestamp: number;
	/**
	 * Class key. Released by flush() once the record leaves the queue unpaired,
	 * whether it was evicted by the ring or simply never matched with a success.
	 */
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
 * Per-family counts and the method behind them live in TODO-misc.md ("ErrorTracker's
 * transient gate"), not here: a number duplicated in a comment is a number nobody
 * can re-derive.
 */
const TRANSIENT_PATTERNS: RegExp[] = [
	/RETRYABLE —/,
	/PARTIAL APPLY —/,
	/fatal: not a git repository/,
];

/**
 * Call-shape rejections: usually noise about how the agent addressed some other
 * server's tool. NOT applied to the alexandria server's own tools — a rejection
 * from `mcp__alexandria__store_memory` is a constraint of THIS product's API, and
 * it is exactly the "error -> what worked" lesson this module exists to write.
 *
 * `Failed to call tool: Missing …` belongs here, not in TRANSIENT_PATTERNS: it is the
 * same family (a call rejected for its arguments), and in TRANSIENT_PATTERNS it was
 * evaluated before the exemption, so alexandria's own parameter contract could never
 * be exempted. `/Edit without read/` was deleted from TRANSIENT_PATTERNS rather than
 * kept: pi-lens's actual wording carries no ERROR_SIGNAL word, so every occurrence
 * died at the signal stage first and the rule guarded nothing.
 */
const CALL_SHAPE_PATTERNS: RegExp[] = [
	/Validation failed for tool/,
	/failed to deserialize parameters/i,
	/Failed to call tool: Missing/,
];

/**
 * Whether this error is about THIS server, judged from the message rather than the
 * tool name alone: gateway traffic arrives with toolName `mcp` and the real call
 * inside its arguments, so a toolName-only exemption would drop this server's own API
 * contract.
 *
 * Insurance, not a measured fix. A replay of the session corpus found no row where
 * this changes the outcome — every `Validation failed for tool "X"` row carried X as
 * the event's own tool name, and pi does emit the inner namespaced name for its nested
 * `ctx.executeTool()` calls. It stays because the adapter-2.x shape
 * (`alexandria_store_memory`) is a real gateway naming and because the failure mode
 * without it is silent evidence loss. The `alexandria_[a-z_]+` alternative also
 * matches this repo's own crate names in bash output, so it is pinned by a test rather
 * than left implicit.
 */
function referencesAlexandria(errorText: string): boolean {
	return /mcp__alexandria__|alexandria_[a-z_]+/.test(errorText);
}

/**
 * A path lookup that found nothing is the routine answer to a wrong guess, and pi
 * emits it as the whole message (`Path not found: <path>`, sometimes prefixed with
 * "Error: "). Matched at the HEAD only: the same words inside another tool's
 * failure — "alexandria serve failed: Path not found: ~/.pi/agent/extensions/
 * alexandria" — are the detail of a durable lesson.
 *
 * Two bounds this comment used to deny, so they are stated here instead:
 *
 * 1. Keying transience on the *tool* was worse — it made every grep/find/ls error
 *    transient at once. Removing the allowlist restores only the failures that carry
 *    an error signal: pi's `Failed to run ripgrep: …` reaches the gate, but
 *    `Not a directory: …` and `rg: the literal "\n" is not allowed in a regex` do
 *    not, because they have no signal word and are dropped by ERROR_SIGNAL first.
 *    That screen, not this one, is the module's largest filter.
 * 2. A blob-wrapped `Path not found:` payload is stored as one row of noise.
 *
 * Re-derive the emission shapes with
 * `grep -o '`[^`]*`' <pi>/dist/core/tools/{grep,ls,find}.js`.
 */
const PATH_LOOKUP_FAILED = /^\s*(Error: )?Path not found:/;

function isTransient(toolName: string, errorText: string): boolean {
	if (PATH_LOOKUP_FAILED.test(errorText)) return true;
	if (TRANSIENT_PATTERNS.some((re) => re.test(errorText))) return true;
	if (toolName.includes("alexandria") || referencesAlexandria(errorText)) return false;
	return CALL_SHAPE_PATTERNS.some((re) => re.test(errorText));
}

/**
 * Whitespace collapse, and nothing else.
 *
 * Four cuts of "elide the payload" were tried across three review rounds, and all
 * four damaged real rows: backticks >= 8 chars and quoted runs >= 24 collapsed
 * ``cannot find function `parse_tags` `` with ``…`store_batch` ``; any parenthesised
 * run >= 16 chars collapsed `(expected Vec<Memory>, found String)` against its exact
 * opposite; the `(ident[N])` span rule merged `(tags[0])` with `(content[0])`; and
 * the survivor — a quoted run inside parentheses — fired on almost nothing, and on
 * none of the `oldText ("…")` rows it existed for (those die at the transient gate
 * first), while mangling durable bash output like
 * `bail!("SteamCMD failed after {attempts}")`. A replay of the session corpus put
 * that at a couple of rows out of ~950 survivors; the script is not committed, so no
 * figure is quoted here — TODO-misc.md says why that matters.
 *
 * So there is no elision. Every rule that made classes coalesce also ate the
 * discriminator, and a merged key costs the second lesson for the whole session via
 * `seen`. Coarser keys are tolerable; wrong ones are not.
 */
function normalizeErrorText(text: string): string {
	return text.replace(/\s+/g, " ").trim();
}

/** Error text must contain at least one of these to be worth tracking. */
const ERROR_SIGNAL_PATTERN =
	/\b(error|fail(ed|ure)?|exception|panic|denied|not found|timeout|refused|abort|crash|fatal|invalid|cannot|couldn'?t|unable|unexpected|broken|missing|violation)\b/i;

export class ErrorTracker {
	private errors: ErrorRecord[] = [];
	private resolutions: Resolution[] = [];
	/** Normalized class keys already tracked this session, so a class is stored once. */
	private seen = new Set<string>();
	/** Keys of ring-evicted records, released at the next flush. */
	private pendingRelease: string[] = [];

	/**
	 * Record a tool error. Caller should pass pre-extracted text
	 * (not the raw MCP response blob).
	 */
	recordError(toolName: string, text: string): void {
		const raw = text.slice(0, MAX_TEXT_LENGTH).trim();

		// Length, signal and transience are all judged on the untouched text. Eliding
		// first let a transient marker hide inside a payload the cut removed, so the
		// gate stored noise it existed to drop; it could also shrink a real error below
		// the length floor and delete it outright.
		if (raw.length < MIN_ERROR_LENGTH) return;
		if (!ERROR_SIGNAL_PATTERN.test(raw)) return;
		if (isTransient(toolName, raw)) return;

		const errorText = normalizeErrorText(raw);

		// Filter: this class is already queued or already stored this session
		const key = `${toolName}\u0000${errorText}`;
		if (this.seen.has(key)) return;
		this.seen.add(key);

		// Ring buffer — drop oldest if full. The claim is released only at the next
		// flush, not immediately: re-recording within the same turn would otherwise
		// displace the neighbour the eviction pushed along and lose a still-pairable
		// lesson, while keeping the claim forever would lose the evicted class instead.
		if (this.errors.length >= MAX_ERRORS) {
			const evicted = this.errors.shift();
			if (evicted) this.pendingRelease.push(evicted.key);
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
		// a real fix, is still a lesson worth storing. Evicted keys go the same way
		// (see recordError): they are unreachable via `errors` by then. A class that
		// DID produce a memory keeps its claim, which is what stops the flood.
		for (const e of this.errors) this.seen.delete(e.key);
		for (const key of this.pendingRelease) this.seen.delete(key);
		this.pendingRelease = [];

		this.resolutions = [];
		this.errors = [];
		return memories;
	}
}
