import test from "node:test";
import assert from "node:assert/strict";
import { detectCorrection } from "../src/detectors/correction.js";
import { SessionDedupBuffer } from "../src/detectors/types.js";

const detect = (prompt: string) =>
	detectCorrection(prompt, new SessionDedupBuffer());

test("each correction pattern captures the corrected statement", () => {
	const cases: Array<[string, string]> = [
		["No, use tabs not spaces.", "tabs not spaces"],
		["That's wrong, the port is 8080.", "the port is 8080"],
		["Actually, the config lives under XDG.", "the config lives under XDG"],
		["I meant the storage crate.", "the storage crate"],
		["Not sqlite, use surrealdb please.", "surrealdb please"],
		["Wrong - the default is 300 seconds.", "the default is 300 seconds"],
		["Incorrect — retries are off by default!", "retries are off by default"],
	];
	for (const [prompt, expected] of cases) {
		assert.deepEqual(
			detect(prompt),
			{ content: `User correction: ${expected}`, tags: ["correction", "auto-detected", "source:regex"] },
			prompt,
		);
	}
});

test("returns null when nothing matches", () => {
	assert.equal(detect("Please add a test for the parser."), null);
	assert.equal(detect("Use ripgrep instead of grep."), null);
	// Used to store "User correction: spaces", a noun with no predicate. It is a prohibition, and
	// the preference detector stores the whole sentence.
	assert.equal(detect("Don't use tabs, use spaces."), null);
});

// No under-8 case: the shortest prompt whose capture survives the two rules below is longer than
// that, so the lower bound cannot be observed here. detectors-preference.test.ts covers it.
test("skips prompts over 500 characters", () => {
	assert.equal(detect(`Actually, ${"x ".repeat(250)}`), null); // spaced, so only the length guard rejects it
});

test("skips a corrected statement shorter than 5 characters", () => {
	assert.equal(detect("No, use a b"), null); // two words, so only the length rule rejects it
});

test("skips a one-word corrected statement", () => {
	// "no, it's completed" is the user reporting state, not a fact worth keeping;
	// the live corpus had 14 copies of "User correction: completed" (A3 probe).
	assert.equal(detect("No, it's completed."), null);
});

test("dedups repeated corrections within a session", () => {
	const buffer = new SessionDedupBuffer();
	assert.notEqual(detectCorrection("Actually, the port is 8080.", buffer), null);
	assert.equal(detectCorrection("actually,  THE port is 8080", buffer), null);
});
