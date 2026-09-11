import test from "node:test";
import assert from "node:assert/strict";
import { detectPreference } from "./preference.ts";
import { detectCorrection } from "./correction.ts";
import { SessionDedupBuffer } from "./types.ts";

const detect = (prompt: string) =>
	detectPreference(prompt, new SessionDedupBuffer());

test("each preference pattern captures the statement", () => {
	const cases: Array<[string, string]> = [
		["Always run just lint before pushing.", "run just lint before pushing"],
		["Never commit Cargo.lock here.", "commit Cargo.lock here"],
		["I prefer tabs over spaces.", "tabs over spaces"],
		["I like ripgrep better than grep.", "ripgrep"],
		["Default to the stable toolchain.", "the stable toolchain"],
		["Don't ever force push main.", "force push main"],
		["Make sure to run the tests.", "run the tests"],
		["From now on, use conventional commits.", "use conventional commits"],
		["Going forward, keep PRs small!", "keep PRs small"],
		["Use ripgrep instead of grep.", "Use ripgrep instead of grep"],
	];
	for (const [prompt, expected] of cases) {
		assert.deepEqual(
			detect(prompt),
			{ content: `User preference: ${expected}`, tags: ["preference", "auto-detected"] },
			prompt,
		);
	}
});

test("returns null when nothing matches", () => {
	assert.equal(detect("Please add a test for the parser."), null);
});

test("skips prompts under 8 or over 500 characters", () => {
	assert.equal(detect("always x"), null);
	assert.equal(detect(`Always ${"x".repeat(500)}`), null);
});

test("dedups repeated preferences within a session", () => {
	const buffer = new SessionDedupBuffer();
	assert.notEqual(detectPreference("Always run the tests.", buffer), null);
	assert.equal(detectPreference("ALWAYS run  the tests", buffer), null);
});

test("'use X instead of Y' is a preference, not a correction", () => {
	const buffer = new SessionDedupBuffer();
	const prompt = "Use ripgrep instead of grep.";
	assert.equal(detectCorrection(prompt, buffer), null);
	assert.equal(
		detectPreference(prompt, buffer)?.content,
		"User preference: Use ripgrep instead of grep",
	);
});
