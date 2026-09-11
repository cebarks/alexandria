import test from "node:test";
import assert from "node:assert/strict";
import { parseExtractionResponse, serializeEntries } from "./extraction-parse.ts";

const msg = (role: string, content: unknown) => ({ type: "message", message: { role, content } });

test("serializes user and assistant text with turn numbers", () => {
	const out = serializeEntries([
		msg("user", "hi"),
		msg("assistant", [
			{ type: "text", text: "hello" },
			{ type: "tool_use", name: "Bash" },
		]),
		msg("user", [{ type: "text", text: "again" }]),
	]);
	assert.equal(out, "[Turn 1 - User]: hi\n\n[Turn 1 - Assistant]: hello\n\n[Turn 2 - User]: again");
});

test("includes compaction summaries in both shapes", () => {
	const out = serializeEntries([
		{ type: "compaction", summary: "flat" },
		{ type: "compaction", compaction: { summary: "nested" } },
	]);
	assert.equal(out, "[Session Summary]: flat\n\n[Session Summary]: nested");
});

test("skips unknown entries, missing messages, and empty text", () => {
	const out = serializeEntries([
		{ type: "tool_result" },
		{ type: "message" },
		msg("user", ""),
		msg("assistant", [{ type: "tool_use" }]),
		msg("user", "kept"),
	]);
	// An empty user message still advances the turn counter.
	assert.equal(out, "[Turn 2 - User]: kept");
});

test("parses bare and fenced JSON", () => {
	const json = '{"memories":[{"content":"a","tags":["t"]}]}';
	const expected = [{ content: "a", tags: ["t"] }];
	assert.deepEqual(parseExtractionResponse(json), expected);
	assert.deepEqual(parseExtractionResponse(`\`\`\`json\n${json}\n\`\`\``), expected);
	assert.deepEqual(parseExtractionResponse(`\`\`\`\n${json}\n\`\`\`\n`), expected);
});

test("defaults missing tags, filters non-string tags, drops empty content", () => {
	const out = parseExtractionResponse(
		'{"memories":[{"content":"a"},{"content":"b","tags":["x",1]},{"content":""},{"tags":["y"]}]}',
	);
	assert.deepEqual(out, [
		{ content: "a", tags: ["extracted"] },
		{ content: "b", tags: ["x"] },
	]);
});

test("returns [] for empty, malformed, or wrongly shaped responses", () => {
	assert.deepEqual(parseExtractionResponse(""), []);
	assert.deepEqual(parseExtractionResponse("not json"), []);
	assert.deepEqual(parseExtractionResponse('{"memories":"x"}'), []);
	assert.deepEqual(parseExtractionResponse("[]"), []);
});
