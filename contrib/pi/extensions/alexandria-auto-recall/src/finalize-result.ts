/**
 * Reads the body of a store_memory or finalize_session result. The server
 * reports failures as a normal text result ({"status":"error","message":...}),
 * never as an MCP error, so the client has to look. No runtime imports: the
 * tests load this module directly.
 */

/** Returns the error message from a tool result body, or undefined when it reports success. */
function resultError(tool: string, text: string | undefined): string | undefined {
	if (text === undefined) return `empty ${tool} result`;
	let body: { status?: unknown; message?: unknown };
	try {
		body = JSON.parse(text);
	} catch {
		return `unparseable ${tool} result: ${text}`;
	}
	if (body.status !== "error") return undefined;
	return String(body.message);
}

/** Returns the message to surface, or undefined when the result needs no attention. */
export function finalizeError(text: string | undefined): string | undefined {
	const message = resultError("finalize_session", text);
	// A chat-only session was never created server-side, so this is the
	// expected outcome, not a fault.
	if (message?.startsWith("Session not found")) return undefined;
	return message;
}

/** Returns the message to surface, or undefined when the store succeeded. */
export function storeError(text: string | undefined): string | undefined {
	return resultError("store_memory", text);
}
