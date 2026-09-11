/**
 * Session fields sent with every store_memory call so auto-store writes are
 * grouped under pi's session in Alexandria. No runtime imports: the tests
 * load this module directly.
 */

export interface SessionArgs {
	session_id: string;
	agent_id: "pi";
	model?: string;
}

/** Minimal ctx shape — avoids importing full pi types as a runtime dependency. */
interface SessionContext {
	sessionManager: { getSessionId(): string };
	model: { id: string } | undefined;
}

export function sessionArgs(ctx: SessionContext): SessionArgs {
	const args: SessionArgs = {
		session_id: ctx.sessionManager.getSessionId(),
		agent_id: "pi",
	};
	if (ctx.model) args.model = ctx.model.id;
	return args;
}
