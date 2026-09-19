import test from "node:test";
import assert from "node:assert/strict";
import { sessionArgs } from "../src/session-args.js";

// These pin what goes onto the wire, nothing more. Whether the server keeps agent_id and model is
// its side of the contract (StoreMemoryParams and SessionRepo in the sessions PR, #11); a server
// without those fields drops them silently and still groups by session_id.
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

// store_memory failures come back as a normal tool result flagged isError whose text block is
// {"status":"error","message":...} (rmcp's structured_error), not as a thrown MCP error.
process.env.ALEXANDRIA_CLIENT_CONFIG ??= "/tmp/alexandria-tests-absent-client.toml";
const { toolErrorMessage } = await import("../src/mcp-client.js");

test("a result is a failure only when the server flags it", () => {
	const body = (o: unknown) => [{ type: "text", text: JSON.stringify(o) }];
	assert.equal(toolErrorMessage({ isError: true, content: body({ status: "error", message: "content is empty" }) }), "content is empty");
	assert.equal(toolErrorMessage({ isError: true, content: [{ type: "text", text: "plain text" }] }), "plain text");
	assert.equal(toolErrorMessage({ isError: true }), "tool call failed");
	// Not flagged: success, whatever the body looks like.
	assert.equal(toolErrorMessage({ content: body({ status: "ok", id: "fact:abc" }) }), undefined);
	assert.equal(toolErrorMessage({ isError: false, content: [] }), undefined);
	assert.equal(toolErrorMessage({ isError: false, content: [{ type: "text", text: "not json" }] }), undefined);
});
