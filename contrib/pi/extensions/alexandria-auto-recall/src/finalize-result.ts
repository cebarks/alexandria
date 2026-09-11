/**
 * Reads the body of a finalize_session result. The server reports failures
 * as a normal text result ({"status":"error","message":...}), never as an
 * MCP error, so the client has to look. No runtime imports: the tests load
 * this module directly.
 */

/** Returns the message to surface, or undefined when the result needs no attention. */
export function finalizeError(text: string | undefined): string | undefined {
	if (text === undefined) return "empty finalize_session result";
	let body: { status?: unknown; message?: unknown };
	try {
		body = JSON.parse(text);
	} catch {
		return `unparseable finalize_session result: ${text}`;
	}
	if (body.status !== "error") return undefined;
	const message = String(body.message);
	// A chat-only session was never created server-side, so this is the
	// expected outcome, not a fault.
	if (message.startsWith("Session not found")) return undefined;
	return message;
}
