import test from "node:test";
import assert from "node:assert/strict";
import { ErrorTracker } from "../src/detectors/error-tracker.js";

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

test("an empty success does not consume the error", () => {
	const t = new ErrorTracker();
	t.recordError("Bash", ERR);
	t.recordSuccess("Bash", "   ");
	t.recordSuccess("Bash", "fixed");
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

// Transient classes below are taken verbatim from the live store's top families:
// 526 error-resolution rows collapsing to 396 classes, 351 of them singletons.
// Every one of these is the tool telling us our own call was malformed or drifted,
// which the retry already resolves — not a durable lesson.
const TRANSIENT: Array<[string, string]> = [
	[
		"edit",
		"🔄 RETRYABLE — Edit target not found\n\nedits[0].oldText (\"use bevy_falling_sand::prelude::{ ChunkRegion,\") was not found in the current file content.",
	],
	[
		"edit",
		"⚠️ PARTIAL APPLY — 1 edit committed (edits[0]). Do NOT resubmit the applied edits.\n\nedits[1].oldText (\"vec![PublishedFileId(1)]\") was not found",
	],
	[
		"ask_user",
		'Validation failed for tool "ask_user":\n  - questions.0.type: must have required properties type, prompt',
	],
	["grep", "Path not found: /home/anten/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/surrealdb-3.2.4/src"],
	["bash", "fatal: not a git repository (or any parent up to mount point /) Stopping at filesystem limit"],
	["mcp", "Failed to call tool: Missing url Expected parameters: category (string) - Optional filter"],
	["edit", '{"content":[{"type":"text","text":"🔄 RETRYABLE — Edit without read\\n\\nYou are trying to edit without reading"}]}'],
];

test("does not memorise transient tool-misuse errors", () => {
	for (const [tool, text] of TRANSIENT) {
		const t = new ErrorTracker();
		t.recordError(tool, text);
		t.recordSuccess(tool, "ok, resolved on retry");
		assert.deepEqual(t.flush(), [], `${tool}: transient error should not be stored`);
	}
});

test("still keeps an error that is a real root cause", () => {
	const t = new ErrorTracker();
	t.recordError(
		"bash",
		"error[E0277]: the trait bound `f16: Polygon` is not satisfied — diskann-wide NEON intrinsics fail trait inference on nightly aarch64",
	);
	t.recordSuccess("bash", "test result: ok. 129 passed");
	const out = t.flush();
	assert.equal(out.length, 1);
	assert.match(out[0].content, /diskann-wide/);
});

test("elides quoted payload so an error class collapses to one row", () => {
	const t = new ErrorTracker();
	t.recordError("read", "Error: cannot parse config: unexpected token in block (alpha = 1\nbeta = 2) at line 4");
	t.recordSuccess("read", "quoted the value");
	t.recordError("read", "Error: cannot parse config: unexpected token in block (gamma = 9\ndelta = 7) at line 4");
	t.recordSuccess("read", "quoted the value");
	const out = t.flush();
	assert.equal(out.length, 1, "same class, different payload must dedup to one memory");
	assert.doesNotMatch(out[0].content, /alpha = 1|gamma = 9/);
	assert.match(out[0].content, /<elided>/);
});
