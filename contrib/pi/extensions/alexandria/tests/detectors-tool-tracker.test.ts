import test from "node:test";
import assert from "node:assert/strict";
import { trackToolStore } from "../src/detectors/tool-tracker.js";
import { SessionDedupBuffer } from "../src/detectors/types.js";

const call = (toolName: string, content: unknown = "fact", isError = false) => {
	const buffer = new SessionDedupBuffer();
	const recorded = trackToolStore({ toolName, input: { content }, isError }, buffer);
	return { recorded, stored: [...buffer.getAllStoredContents()] };
};

test("records successful store_memory and update_memory calls by suffix", () => {
	assert.deepEqual(call("alexandria_store_memory"), { recorded: true, stored: ["fact"] });
	assert.deepEqual(call("mcp__alexandria__update_memory"), { recorded: true, stored: ["fact"] });
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
	assert.deepEqual(call("alexandria_retrieve_memories"), { recorded: false, stored: [] });
	assert.deepEqual(call("alexandria_store_memory", "fact", true), { recorded: false, stored: [] });
	assert.deepEqual(call("alexandria_store_memory", 42), { recorded: false, stored: [] });
	assert.deepEqual(call("alexandria_store_memory", ""), { recorded: false, stored: [] });
});
