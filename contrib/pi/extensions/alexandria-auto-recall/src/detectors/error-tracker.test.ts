import test from "node:test";
import assert from "node:assert/strict";
import { ErrorTracker } from "./error-tracker.ts";

const ERR = "Error: command not found: cargo-deny, install it first";

test("pairs an error with the next success on the same tool", () => {
	const t = new ErrorTracker();
	t.recordError("Bash", ERR);
	t.recordSuccess("Bash", "cargo deny check passed");
	assert.deepEqual(t.flush(), [
		{
			content: `Error with Bash: ${ERR}\nResolution: cargo deny check passed`,
			tags: ["error-resolution", "auto-detected", "Bash"],
		},
	]);
	assert.deepEqual(t.flush(), []);
});

test("a paired error is consumed", () => {
	const t = new ErrorTracker();
	t.recordError("Bash", ERR);
	t.recordSuccess("Bash", "first");
	t.recordSuccess("Bash", "second");
	assert.equal(t.flush().length, 1);
});

test("success on a different tool does not pair", () => {
	const t = new ErrorTracker();
	t.recordError("Bash", ERR);
	t.recordSuccess("Read", "file contents");
	assert.deepEqual(t.flush(), []);
});

test("drops short errors and text without an error signal", () => {
	const t = new ErrorTracker();
	t.recordError("Bash", "error: nope");
	t.recordError("Bash", "the quick brown fox jumps over the lazy dog twice");
	t.recordSuccess("Bash", "ok");
	assert.deepEqual(t.flush(), []);
});

test("truncates error and success text to 200 characters", () => {
	const t = new ErrorTracker();
	t.recordError("Bash", `error: ${"a".repeat(300)}`);
	t.recordSuccess("Bash", "b".repeat(300));
	const [m] = t.flush();
	assert.equal(m.content, `Error with Bash: error: ${"a".repeat(193)}\nResolution: ${"b".repeat(200)}`);
});

test("keeps only the five newest errors", () => {
	const t = new ErrorTracker();
	for (let i = 0; i < 6; i++) t.recordError(`tool${i}`, `${ERR} ${i}`);
	t.recordSuccess("tool0", "fixed");
	t.recordSuccess("tool1", "fixed");
	t.recordSuccess("tool5", "fixed");
	assert.deepEqual(
		t.flush().map((m) => m.tags[2]),
		["tool1", "tool5"],
	);
});

test("flush clears unpaired errors", () => {
	const t = new ErrorTracker();
	t.recordError("Bash", ERR);
	t.flush();
	t.recordSuccess("Bash", "fixed");
	assert.deepEqual(t.flush(), []);
});
