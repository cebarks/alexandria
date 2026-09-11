import test from "node:test";
import assert from "node:assert/strict";
import { detectCorrection } from "./correction.ts";
import { SessionDedupBuffer } from "./types.ts";

const detect = (prompt: string) =>
	detectCorrection(prompt, new SessionDedupBuffer());

test("each correction pattern captures the corrected statement", () => {
	const cases: Array<[string, string]> = [
		["No, use tabs not spaces.", "tabs not spaces"],
		["That's wrong, the port is 8080.", "the port is 8080"],
		["Actually, the config lives under XDG.", "the config lives under XDG"],
		["I meant the storage crate.", "the storage crate"],
		["Not sqlite, use surrealdb please.", "surrealdb please"],
		["Don't use tabs, use spaces for indent.", "spaces for indent"],
		["Wrong - the default is 300 seconds.", "the default is 300 seconds"],
		["Incorrect — retries are off by default!", "retries are off by default"],
	];
	for (const [prompt, expected] of cases) {
		assert.deepEqual(
			detect(prompt),
			{ content: `User correction: ${expected}`, tags: ["correction", "auto-detected"] },
			prompt,
		);
	}
});

test("returns null when nothing matches", () => {
	assert.equal(detect("Please add a test for the parser."), null);
	assert.equal(detect("Use ripgrep instead of grep."), null);
});

test("skips prompts under 8 or over 500 characters", () => {
	assert.equal(detect("no, use"), null);
	assert.equal(detect(`Actually, ${"x".repeat(500)}`), null);
});

test("skips a corrected statement shorter than 5 characters", () => {
	assert.equal(detect("No, use tabs"), null);
});

test("dedups repeated corrections within a session", () => {
	const buffer = new SessionDedupBuffer();
	assert.notEqual(detectCorrection("Actually, the port is 8080.", buffer), null);
	assert.equal(detectCorrection("actually,  THE port is 8080", buffer), null);
});
