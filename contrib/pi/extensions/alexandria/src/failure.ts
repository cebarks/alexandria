/**
 * Failure attribution for the prompt path.
 *
 * Pure by construction: nothing here reads CONFIG, touches the network, or imports
 * the MCP SDK. Like `injection.ts`, it exists as a separate module because the
 * `before_agent_start` handler is an inline closure over imported functions and
 * cannot be reached from a test without module mocking, which hangs under tsx.
 */

/**
 * Cap on cause-chain depth. undici nests one or two levels; anything deeper is a
 * cycle or a library bug, and neither deserves more text in a toast.
 */
const MAX_CAUSE_DEPTH = 6;

/**
 * The user-facing form of a rejection: the **deepest** cause's message, with its
 * `code` appended when it has one.
 *
 * `err.message` alone is actively misleading for transport failures. undici wraps
 * every connection error in a `TypeError: fetch failed` and puts the real reason on
 * `cause`, so reading only the wrapper renders a closed port, a dead DNS name and
 * an unroutable host as the identical string "fetch failed" — the operator learns
 * nothing, and the one detail that would distinguish "the server is down" from
 * "this hostname does not resolve from here" is thrown away.
 *
 * Deepest wins because that is where the OS-level truth lives: the wrappers are
 * added by layers that only know a call failed, not why.
 *
 * A `seen` set rather than the depth cap alone: a cyclic `cause` is legal JS, and
 * looping forever on the path pi awaits before starting a turn would be worse than
 * the failure being diagnosed.
 */
export function describeCause(err: unknown): string {
	const parts: string[] = [];
	const seen = new Set<unknown>();
	let cur: unknown = err;

	for (let i = 0; i < MAX_CAUSE_DEPTH && cur !== undefined && cur !== null; i++) {
		if (seen.has(cur)) break;
		seen.add(cur);
		if (!(cur instanceof Error)) {
			// A thrown string, or a DOMException-ish object from another realm:
			// still worth showing, and `String()` cannot throw here.
			parts.push(String(cur));
			break;
		}
		const coded = cur as Error & { code?: unknown };
		parts.push(
			typeof coded.code === "string" && coded.code !== ""
				? `${cur.message} [${coded.code}]`
				: cur.message,
		);
		cur = (cur as Error & { cause?: unknown }).cause;
	}

	return parts.length === 0 ? "unknown error" : parts[parts.length - 1];
}

/** Which component actually failed. Drives both the wording and whether the
 *  shared connection gets dropped. Every variant has a producer: `cancelled` and
 *  `stalled` come from the two abort sources in `index.ts`, `transport` is the
 *  fallback for anything the server or the network did. */
export type FailureKind = "cancelled" | "stalled" | "transport";

export interface FailureContext {
	/** The prompt signal was aborted: user interrupt, superseded prompt, reload. */
	aborted: boolean;
	/** The whole-prompt budget expired, rather than a per-call server deadline. */
	budgetExceeded?: boolean;
}

export interface ClassifiedFailure {
	kind: FailureKind;
	/** Rendered cause, from {@linkcode describeCause}. */
	cause: string;
	/** Whether the shared connection should be dropped. */
	resetConnection: boolean;
}

/**
 * Attribute a rejection to the component that actually failed.
 *
 * The ordering is the whole point. `aborted` wins over everything because the SDK
 * labels an aborted request `REQUEST_TIMEOUT`, so the error text alone would report
 * a benign Esc as a dead server *and* reset a healthy session. `budgetExceeded`
 * comes next: the per-call deadline (5 s) is shorter than the whole-path budget
 * (10 s), so a genuinely slow server trips the per-call timer first and arrives here
 * as a transport failure. Reaching the budget instead means the timers themselves
 * ran late — pi's event loop stalled — which the diagnostic log's `driftMs` field is
 * there to corroborate.
 *
 * `resetConnection` is false for `cancelled` and `stalled` deliberately. In both the
 * connection is fine, and dropping it costs a fresh handshake on the next prompt
 * while fixing nothing — the old `resetClient = failure !== null` conflated "the
 * server is gone" with "the user pressed Esc".
 */
export function classifyFailure(
	err: unknown,
	ctx: FailureContext,
): ClassifiedFailure {
	const cause = err instanceof AlexandriaFailure ? err.message : describeCause(err);
	if (ctx.aborted) return { kind: "cancelled", cause, resetConnection: false };
	if (ctx.budgetExceeded) return { kind: "stalled", cause, resetConnection: false };
	return { kind: "transport", cause, resetConnection: true };
}

/**
 * A rejection that already knows what it was.
 *
 * The tasks in `index.ts` classify at the throw site, where the signal state and
 * the bridge's fault flag are still in scope; `buildInjection` then only reads the
 * result. Without this the merge step would have to re-derive context it cannot
 * see — it receives a settled promise and nothing else.
 */
export class AlexandriaFailure extends Error {
	readonly kind: FailureKind;
	readonly resetConnection: boolean;

	constructor(message: string, opts: { kind: FailureKind; resetConnection: boolean }) {
		super(message);
		this.name = "AlexandriaFailure";
		this.kind = opts.kind;
		this.resetConnection = opts.resetConnection;
	}
}

/**
 * Read the classification off a rejection, falling back to classifying it as a
 * transport failure.
 *
 * The fallback is the pre-existing behaviour on purpose: an unwrapped rejection
 * must not silently lose its connection reset while throw sites are migrated one
 * at a time.
 */
export function failureOf(reason: unknown): ClassifiedFailure {
	if (reason instanceof AlexandriaFailure) {
		return {
			kind: reason.kind,
			cause: reason.message,
			resetConnection: reason.resetConnection,
		};
	}
	return classifyFailure(reason, { aborted: false });
}

/**
 * The throw-site half of the contract: classify and wrap in one step.
 *
 * This exists as a function rather than two inline lines in `index.ts` because the
 * handler is an inline closure over imported functions and cannot be reached from
 * a test without module mocking, which hangs under tsx. The decision that matters
 * most here — that an operator's Esc is not a server failure and must not tear down
 * the session — is otherwise assertable only by reading the handler. Extracting it
 * makes it a table test.
 */
export function asFailure(err: unknown, ctx: FailureContext): AlexandriaFailure {
	const c = classifyFailure(err, ctx);
	return new AlexandriaFailure(c.cause, {
		kind: c.kind,
		resetConnection: c.resetConnection,
	});
}
