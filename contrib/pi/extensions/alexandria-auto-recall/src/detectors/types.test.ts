import test from "node:test";
import assert from "node:assert/strict";
import { SessionDedupBuffer } from "./types.ts";

test("heuristic stores dedup on case and whitespace, keeping the first raw form", () => {
	const b = new SessionDedupBuffer();
	assert.equal(b.addHeuristicStore("Foo  bar"), true);
	assert.equal(b.addHeuristicStore(" foo BAR "), false);
	assert.deepEqual(b.getAllStoredContents(), ["Foo  bar"]);
});

test("tool stores are appended without dedup", () => {
	const b = new SessionDedupBuffer();
	b.addToolStore("x");
	b.addToolStore("x");
	assert.deepEqual(b.getAllStoredContents(), ["x", "x"]);
});
