/**
 * The recall block's rendering, hardened to the same discipline as the reminder
 * block: both are joined into one injected message, so a leak in either one lands
 * in the agent's context.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { join } from "node:path";
import { writeFileSync } from "node:fs";
import { makeScratchDir } from "./helpers.js";

process.env.ALEXANDRIA_CLIENT_CONFIG ??= "/tmp/alexandria-tests-absent-client.json";

const { formatMemoriesBlock, retrieveMemories } = await import("../src/recall.js");
const { createRecallSettingsStore } = await import("../src/config.js");
type CallTool = import("../src/mcp-client.js").CallTool;

const callWith = (payload: unknown): CallTool => async () => ({
	content: [{ type: "text", text: JSON.stringify(payload) }],
});

test("one bullet per memory, newlines collapsed, no undefined leaking", () => {
	const block = formatMemoriesBlock([
		// A missing id is what an agent would otherwise be told to pass to
		// update_memory, so it has to read as unknown rather than as the literal
		// word "undefined" — the exact leak the reminder path was hardened against.
		{ content: "prefers  just\n over  spaces", similarity: 0.91 },
		{ id: "fact:abc", content: "a decision", similarity: 0.7, tags: ["a", "b"] },
	] as never);
	const lines = block.split("\n");
	assert.match(lines[0], /Relevant memories retrieved automatically/);
	assert.equal(lines[1], "- (similarity 0.91, id ?) prefers  just over  spaces");
	assert.equal(
		lines[2],
		"- (similarity 0.70, id fact:abc) [a, b] a decision",
	);
	assert.doesNotMatch(block, /undefined/);
});

test("a non-numeric similarity scores as n/a instead of throwing", () => {
	// `String(undefined).toFixed` is a TypeError, and a throw in the formatter reads
	// to the user as "auto-recall failed" plus a needless client reset.
	const block = formatMemoriesBlock([
		{ id: "fact:1", content: "fine", similarity: "high" },
	] as never);
	assert.match(block, /similarity n\/a/);
});

test("a payload problem comes back as an error, not as silence", async () => {
	// "The server is broken" and "nothing matched" have to be distinguishable; a
	// bare [] conflated them, and the reminder path already had this channel.
	for (const [payload, expected] of [
		[{ results: "not a list" }, /no results list/],
		[null, /no results list/],
	] as const) {
		const { items, error } = await retrieveMemories("q", callWith(payload));
		assert.deepEqual(items, []);
		assert.match(error ?? "", expected as RegExp);
	}
	const errored = await retrieveMemories("q", async () => ({
		isError: true,
		content: [{ type: "text", text: "index unavailable" }],
	}));
	assert.match(errored.error ?? "", /index unavailable/);
});

test("a pending recall uses its original limit and threshold after settings change", async () => {
	const dir = await makeScratchDir("alexandria-recall-snapshot-");
	const path = join(dir, "client.json");
	writeFileSync(path, JSON.stringify({ recall: { enabled: true, limit: 3, min_similarity: 0.8 } }));
	const store = createRecallSettingsStore(path, {});
	let release!: () => void;
	const wait = new Promise<void>((resolve) => { release = resolve; });
	const limits: unknown[] = [];
	const call: CallTool = async (_name, args) => {
		limits.push(args.limit);
		await wait;
		return { content: [{ type: "text", text: JSON.stringify({ results: [
			{ id: "fact:high", content: "high", similarity: 0.9 },
			{ id: "fact:mid", content: "mid", similarity: 0.6 },
		] }) }] };
	};
	const pending = retrieveMemories("q", call, undefined, store.get().effective);
	store.save("limit", 8);
	store.save("minSimilarity", 0.5);
	release();
	assert.deepEqual((await pending).items.map((item) => item.id), ["fact:high"]);
	assert.deepEqual((await retrieveMemories("q", call, undefined, store.get().effective)).items.map((item) => item.id), ["fact:high", "fact:mid"]);
	assert.deepEqual(limits, [3, 8]);
});

test("a usable result set still filters by the configured floor", async () => {
	const { items, error } = await retrieveMemories(
		"q",
		callWith({
			results: [
				{ id: "fact:keep", content: "strong", similarity: 0.9 },
				{ id: "fact:drop", content: "weak", similarity: 0.1 },
				{ content: "not an object with content only", similarity: "x" },
			],
		}),
	);
	assert.equal(error, undefined);
	assert.deepEqual(
		items.map((m) => m.id),
		["fact:keep"],
	);
});
