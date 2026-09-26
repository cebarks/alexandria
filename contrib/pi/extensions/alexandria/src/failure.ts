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
