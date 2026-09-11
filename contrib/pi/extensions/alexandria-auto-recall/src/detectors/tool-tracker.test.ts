import test from "node:test";
import assert from "node:assert/strict";
import { trackToolStore } from "./tool-tracker.ts";
import { SessionDedupBuffer } from "./types.ts";

const call = (toolName: string, content: unknown = "fact", isError = false) => {
	const buffer = new SessionDedupBuffer();
	const recorded = trackToolStore({ toolName, input: { content }, isError }, buffer);
	return { recorded, stored: [...buffer.getAllStoredContents()] };
};

test("records successful store_memory and update_memory calls by suffix", () => {
	assert.deepEqual(call("alexandria_store_memory"), { recorded: true, stored: ["fact"] });
	assert.deepEqual(call("mcp__alexandria__update_memory"), { recorded: true, stored: ["fact"] });
});

test("ignores other tools, errors, and non-string content", () => {
	assert.deepEqual(call("alexandria_retrieve_memories"), { recorded: false, stored: [] });
	assert.deepEqual(call("alexandria_store_memory", "fact", true), { recorded: false, stored: [] });
	assert.deepEqual(call("alexandria_store_memory", 42), { recorded: false, stored: [] });
	assert.deepEqual(call("alexandria_store_memory", ""), { recorded: false, stored: [] });
});
