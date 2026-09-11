import test from "node:test";
import assert from "node:assert/strict";
import { sessionArgs } from "./session-args.ts";

const ctx = (model: { id: string } | undefined) => ({
	sessionManager: { getSessionId: () => "abc-123" },
	model,
});

test("carries pi's session id, agent_id, and model id", () => {
	assert.deepEqual(sessionArgs(ctx({ id: "claude-haiku-4-5" })), {
		session_id: "abc-123",
		agent_id: "pi",
		model: "claude-haiku-4-5",
	});
});

test("omits model when none is set", () => {
	assert.deepEqual(sessionArgs(ctx(undefined)), {
		session_id: "abc-123",
		agent_id: "pi",
	});
});
