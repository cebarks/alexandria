import test from "node:test";
import assert from "node:assert/strict";
import { detectPreference } from "../src/detectors/preference.js";
import { detectCorrection } from "../src/detectors/correction.js";
import { SessionDedupBuffer } from "../src/detectors/types.js";

const detect = (prompt: string) =>
	detectPreference(prompt, new SessionDedupBuffer());
const stored = (prompt: string) => detect(prompt)?.content;

// The four prompts from the #18 review. The first three used to be stored with the
// negation stripped ("User preference: force push main").
test("a prohibition is stored with its negation", () => {
	assert.equal(stored("Never commit Cargo.lock here."), "User preference: Never commit Cargo.lock here");
	assert.equal(stored("Don't ever force push main."), "User preference: Don't ever force push main");
	assert.equal(
		stored("Never run recursive searches without scoping first."),
		"User preference: Never run recursive searches without scoping first",
	);
	assert.equal(stored("Use jaq instead of jq."), "User preference: Use jaq instead of jq");
});

test("every stored statement starts at its trigger, so nothing before the predicate is stripped", () => {
	const cases: Array<[string, string]> = [
		["Always run just lint before pushing.", "Always run just lint before pushing"],
		["I prefer tabs over spaces.", "I prefer tabs over spaces"],
		["I like ripgrep better than grep.", "I like ripgrep better than grep"],
		["Default to the stable toolchain.", "Default to the stable toolchain"],
		["Make sure to run the tests.", "Make sure to run the tests"],
		["Please don't push without explicit go-ahead.", "don't push without explicit go-ahead"],
		["Do not amend pushed commits!", "Do not amend pushed commits"],
		["Don't use tabs, use spaces.", "Don't use tabs, use spaces"],
		["ok. I don't ever want force pushes", "don't ever want force pushes"],
		// Forward-looking lead-ins carry no polarity; what follows them is the statement.
		["From now on, use conventional commits.", "use conventional commits"],
		["Going forward, never squash review fixes.", "never squash review fixes"],
	];
	for (const [prompt, expected] of cases) {
		assert.deepEqual(
			detect(prompt),
			{ content: `User preference: ${expected}`, tags: ["preference", "auto-detected", "source:regex"] },
			prompt,
		);
	}
});

test("a negation in front of the trigger is never dropped", () => {
	// Negative triggers are tried first, so the whole prohibition is the statement.
	assert.equal(stored("Please don't always rebase onto main."), "User preference: don't always rebase onto main");
	// No pattern can keep this negation, so nothing is stored: an inverted memory is worse than none.
	assert.equal(detect("It's not that I prefer tabs over spaces."), null);
	assert.equal(detect("You shouldn't default to the nightly toolchain."), null);
	assert.equal(detect("There's no need to always rebase first."), null);
	// The guard is per clause: a correction lead-in or an earlier sentence does not veto the statement.
	assert.equal(stored("No, always run clippy first."), "User preference: always run clippy first");
	assert.equal(stored("That is not it. Always run clippy first."), "User preference: Always run clippy first");
});

test("a mid-clause don't is a statement about the speaker, not an instruction", () => {
	assert.equal(detect("I don't know why this test fails."), null);
	assert.equal(detect("We don't have a staging server."), null);
});

test("returns null when nothing matches", () => {
	assert.equal(detect("Please add a test for the parser."), null);
});

test("skips prompts under 8 or over 500 characters", () => {
	assert.equal(detect("never x"), null); // 7 characters; stored without the length guard
	assert.equal(detect(`Always ${"x".repeat(500)}`), null);
});

test("dedups repeated preferences within a session", () => {
	const buffer = new SessionDedupBuffer();
	assert.notEqual(detectPreference("Always run clippy.", buffer), null);
	assert.equal(detectPreference("always  RUN clippy", buffer), null);
});

test("'use X instead of Y' is stored once, as a preference", () => {
	const buffer = new SessionDedupBuffer();
	const prompt = "Use ripgrep instead of grep.";
	assert.equal(detectCorrection(prompt, buffer), null);
	assert.equal(detectPreference(prompt, buffer)?.content, "User preference: Use ripgrep instead of grep");
});
