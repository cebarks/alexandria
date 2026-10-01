import test from "node:test";
import assert from "node:assert/strict";
import { trackToolStore } from "../src/detectors/tool-tracker.js";
import { SessionDedupBuffer } from "../src/detectors/types.js";

const call = (toolName: string, content: unknown = "fact", isError = false) => {
	const buffer = new SessionDedupBuffer();
	const recorded = trackToolStore({ toolName, input: { content }, isError }, buffer);
	return { recorded, stored: [...buffer.getAllStoredContents()] };
};

test("records store_memory and update_memory in either client's naming", () => {
	// pi's built-in MCP (0.99+) and pi-mcp-adapter 2.x (mars) respectively.
	assert.deepEqual(call("mcp__alexandria__store_memory"), { recorded: true, stored: ["fact"] });
	assert.deepEqual(call("alexandria_store_memory"), { recorded: true, stored: ["fact"] });
	assert.deepEqual(call("mcp__alexandria__update_memory"), { recorded: true, stored: ["fact"] });
	assert.deepEqual(call("update_memory"), { recorded: true, stored: ["fact"] });
});

test("ignores another server's tool that merely ends with a store tool name", () => {
	// Pi names every MCP tool mcp__<server>__<tool>, so a suffix match reaches into
	// other servers: `auto_store_memory` on another one is not our store_memory.
	assert.deepEqual(call("mcp__agentmemory__auto_store_memory"), {
		recorded: false,
		stored: [],
	});
	assert.deepEqual(call("mcp__agentmemory__update_memory_backup"), {
		recorded: false,
		stored: [],
	});
});

test("ignores other tools, errors, and non-string content", () => {
	assert.deepEqual(call("mcp__alexandria__retrieve_memories"), { recorded: false, stored: [] });
	assert.deepEqual(call("mcp__alexandria__store_memory", "fact", true), {
		recorded: false,
		stored: [],
	});
	assert.deepEqual(call("mcp__alexandria__store_memory", 42), { recorded: false, stored: [] });
	assert.deepEqual(call("mcp__alexandria__store_memory", ""), { recorded: false, stored: [] });
});

test("KNOWN BOUND: a different server's bare store_memory is still accepted", () => {
	// Pinned deliberately. Scoping to the alexandria server needs its registered name, which differs
	// per client (mcp__alexandria__* on pi >=0.99, alexandria_* on adapter 2.x) and the companion does
	// not know which key an operator chose. Accepted in the PR #41 review; if a second memory server
	// enters the fleet, change the matcher AND this test together.
	assert.deepEqual(call("mcp__agentmemory__store_memory"), { recorded: true, stored: ["fact"] });
	assert.deepEqual(call("bulk_store_memory"), { recorded: true, stored: ["fact"] });
});
