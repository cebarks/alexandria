import type { ExtensionContext } from "@earendil-works/pi-coding-agent";

/**
 * Session fields sent with every store_memory call so auto-store writes are
 * grouped under pi's session in Alexandria.
 */

export interface SessionArgs {
	session_id: string;
	agent_id: "pi";
	model?: string;
}

/**
 * Type-only slice of pi's context, so the fields we reach for are pi's real
 * declarations rather than a shape we guessed.
 */
export type SessionContext = Pick<ExtensionContext, "sessionManager" | "model">;

export function sessionArgs(ctx: SessionContext): SessionArgs {
	const args: SessionArgs = {
		session_id: ctx.sessionManager.getSessionId(),
		agent_id: "pi",
	};
	if (ctx.model) args.model = ctx.model.id;
	return args;
}
