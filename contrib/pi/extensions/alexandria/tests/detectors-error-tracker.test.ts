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

// Fixtures match shapes seen in the live store and in pi / pi-lens emission
// templates — NOT verbatim copies. That distinction matters because a
// "copied verbatim from real rows" claim here was once false: one fixture was an
// invented "Error executing grep: …" string that neither pi nor pi-lens emits, and it
// gave a widened rule coverage it did not deserve. Where an emitter exists, the
// string is checked against its source.
// Per-family counts and their caveats are recorded once in TODO-misc.md
// ("ErrorTracker's transient gate") rather than restated here.
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

// Superseded by the round-2 tests below. This fixture asserted that any
// parenthesised run should be elided, which is the behaviour the review rejected:
// eliding `(alpha = 1, beta = 2)`-shaped content is what collapsed distinct root
// causes together. Both directions are now pinned there — echoed payloads elide,
// diagnostics do not.

// ── Findings from the pre-merge review of PR #41, each reproduced by probe first ──

const pair = (tool: string, err: string, ok = "applied the obvious retry and it worked") => {
	const t = new ErrorTracker();
	t.recordError(tool, err);
	t.recordSuccess(tool, ok);
	return t.flush();
};

test("a class evicted unpaired by flush can pair again", () => {
	const t = new ErrorTracker();
	const E = "cargo build failed: linking with cc returned non-zero exit code 1";
	t.recordError("bash", E);
	t.flush(); // agent_end of the turn: unpaired, so nothing was learned yet
	t.recordError("bash", E);
	t.recordSuccess("bash", "pinning the cc linker in cargo config fixed the build");
	assert.equal(t.flush().length, 1, "the lesson was lost because the class was marked seen at queue time");
});

test("errors differing only in a backticked identifier are both kept", () => {
	const t = new ErrorTracker();
	t.recordError("bash", "error[E0425]: cannot find function `parse_tags` in this scope, add the import");
	t.recordSuccess("bash", "added the import to lib.rs");
	t.recordError("bash", "error[E0425]: cannot find function `store_batch` in this scope, add the import");
	t.recordSuccess("bash", "added the import to lib.rs");
	assert.equal(t.flush().length, 2, "two different missing functions are two different root causes");
});

test("the payload a tool quoted back is not elided out of the length floor", () => {
	const out = pair(
		"bash",
		'Failed to store: "content exceeds the maximum allowed length"',
		"shortened the memory and it stored",
	);
	assert.equal(out.length, 1);
	assert.match(out[0].content, /maximum allowed length/, "the constraint is the lesson");
});

test("the resolution keeps what actually fixed it", () => {
	const out = pair(
		"bash",
		"justfile recipe failed: the CARGO_TARGET_DIR variable is missing from the environment",
		"exported CARGO_TARGET_DIR=`/tmp/cargo-target` in the justfile recipe line",
	);
	assert.equal(out.length, 1);
	assert.match(out[0].content, /CARGO_TARGET_DIR=`\/tmp\/cargo-target`/);
});

test("alexandria's own schema rejections stay trackable", () => {
	const out = pair(
		"mcp__alexandria__store_memory",
		// pi's real format is `Validation failed for tool "${toolCall.name}"` — the
		// namespaced name, not the bare one. A bare-name fixture passes while hiding the
		// gateway case, which is how that hole survived two review rounds.
		'Validation failed for tool "mcp__alexandria__store_memory":\n  - tags: expected string, found array',
		"retried with tags as a newline-separated string and it stored",
	);
	assert.equal(out.length, 1, "this server's own API contract is a durable lesson, not call noise");
});

// Fixtures here use pi 0.99.1's real emission shapes, verified with
// `grep -o '`[^`]*`' <pi>/dist/core/tools/{grep,ls,find}.js`: grep emits
// `Path not found: ${searchPath}` and `Failed to run ripgrep: ${error.message}`; ls
// adds `Cannot read directory: …` and `Not a directory: …`; find emits
// `Failed to run fd: …`. An earlier version of this test asserted on
// "Error executing grep: Path not found: …", a string neither pi nor pi-lens
// produces — it existed only to make a widened rule look covered.
test("a path lookup that found nothing is transient; the same words mid-message are not", () => {
	// The whole message is the marker — pi's actual grep/ls/find output.
	assert.equal(pair("grep", "Path not found: /home/anten/.cargo/registry/src/surrealdb-3.2.4/src").length, 0);
	assert.equal(pair("ls", "Error: Path not found: /home/anten/code/alexandria/crates/alexandria-storage/migrations").length, 0);
	// The marker as a detail inside a different failure is a durable lesson.
	assert.equal(
		pair("bash", "alexandria serve failed: Path not found: /home/anten/.pi/agent/extensions/alexandria").length,
		1,
	);
});

test("a durable reason behind Failed to call tool is kept", () => {
	const out = pair(
		"mcp",
		"Failed to call tool: alexandria MCP server connection refused, memories are not retrievable",
		"restarted it with `alexandria serve` and calls succeed again",
	);
	assert.equal(out.length, 1);
});

test("an error that merely mentions the retry marker is not treated as transient", () => {
	const out = pair(
		"bash",
		"upstream says RETRYABLE later, but the failure now is disk full: no space left on /data",
		"freed space on /data and the write succeeded",
	);
	assert.equal(out.length, 1);
});

test("the marker at the head of a tool-protocol error is still transient", () => {
	assert.equal(pair("edit", "🔄 RETRYABLE — Edit target not found: the file changed since you read it", "re-read then applied").length, 0);
	assert.equal(pair("edit", "⚠️ PARTIAL APPLY — 1 edit committed (edits[0]). Do NOT resubmit the applied edits.", "submitted only the remainder").length, 0);
});

test("pi-lens's Edit-without-read wording is dropped, but not by the gate", () => {
	// pi-lens 4.3.0 emits this at dist/clients/read-guard.js. It carries no
	// ERROR_SIGNAL word, so it dies before isTransient() is reached — which is why a
	// `/Edit without read/` rule was deleted: it guarded nothing, and its only fixture
	// was a JSON blob that also died at the signal stage. The OUTCOME is pinned (the row
	// is dropped); the MECHANISM is not — re-adding `/Edit without read/` fails no test,
	// because `RETRYABLE —` already covers every real row and ERROR_SIGNAL drops the
	// rest. Do not read this test as guarding the deleted rule.
	assert.equal(
		pair(
			"edit",
			"🔄 RETRYABLE — Edit without read: you have not read the file in this conversation. Read it first, then retry",
		).length,
		0,
	);
});

// ── Round 2 of the PR #41 review: the elision cut itself was the defect ──

test("two diagnostics that differ only inside parentheses are both kept", () => {
	const t = new ErrorTracker();
	t.recordError("bash", "error[E0308]: mismatched types (expected Vec<Memory>, found String) in the memory repo signature");
	t.recordSuccess("bash", "swapped the argument order and it compiles");
	t.recordError("bash", "error[E0308]: mismatched types (expected String, found Vec<Memory>) in the memory repo signature");
	t.recordSuccess("bash", "swapped the argument order and it compiles");
	const out = t.flush();
	assert.equal(out.length, 2, "opposite type directions are two different lessons");
	assert.match(out[0].content, /expected Vec<Memory>, found String/);
	assert.match(out[1].content, /expected String, found Vec<Memory>/);
});

test("no payload elision: a durable row keeps its echoed source verbatim", () => {
	const out = pair(
		"bash",
		'cargo build failed: bail!("SteamCMD failed after {STEAMCMD_MAX_ATTEMPTS} attempts") did not compile',
	);
	assert.equal(out.length, 1);
	assert.match(out[0].content, /STEAMCMD_MAX_ATTEMPTS/, "the quoted text is the lesson");
});

test("the transient gate sees a marker hidden inside parentheses", () => {
	assert.equal(
		pair("edit", "Edit failed (PARTIAL APPLY — 2 edits committed, do NOT resubmit the remaining ones)").length,
		0,
	);
});

test("an evicted class does not displace a queued neighbour", () => {
	const t = new ErrorTracker();
	const mk = (n: string) => `build failed for ${n} crate: linker dropped a section, add the missing crate dependency`;
	for (const n of ["alpha", "bravo", "charlie", "delta", "echo"]) t.recordError(`t${n}`, mk(n));
	t.recordError("tzulu", mk("zulu")); // evicts talpha
	t.recordError("talpha", mk("alpha")); // must NOT re-enter and evict tbravo
	t.recordSuccess("tbravo", "added the crate to Cargo.toml and the build passed");
	const out = t.flush();
	assert.equal(out.length, 1);
	assert.match(out[0].content, /bravo/);
});

// ── Round 3 of the PR #41 review: three of these were confirmed by probe before fixing ──

test("other grep/find/ls failures still reach the gate when they carry a signal word", () => {
	// Keying transience on the TOOL made grep/find/ls a lesson desert. Removing that
	// axis restores only the failures that clear ERROR_SIGNAL: pi's `Not a directory:`
	// and `rg: the literal "\n" is not allowed in a regex` still die at the signal
	// stage, which is the structural bound recorded in TODO-misc, not something this
	// test claims to have fixed.
	assert.equal(
		pair("grep", "Failed to run ripgrep: spawn EACCES, the rg binary under ~/.local/bin is not executable").length,
		1,
	);
	assert.equal(pair("ls", "Cannot read directory: EACCES, permission denied on /root/.ssh/config").length, 1);
	assert.equal(pair("find", "Failed to run fd: permission denied on /root/.ssh while searching").length, 1);
});

test("a call-shape rejection is transient unless it names this server", () => {
	// Gateway-routed traffic arrives as toolName "mcp" with the real call in the text,
	// so the exemption has to read the message and not only the tool name.
	assert.equal(
		pair("mcp", "Failed to call tool: Missing url Expected parameters: category (string) - Optional filter").length,
		0,
		"another server's argument error is noise",
	);
	assert.equal(
		pair(
			"mcp",
			'Validation failed for tool "mcp__alexandria__store_memory":\n  - tags: expected string, found array',
		).length,
		1,
		"this server's own contract must survive",
	);
	assert.equal(
		pair(
			"mcp",
			"failed to deserialize parameters: missing field `content` — Expected parameters: content (string) *required*",
		).length,
		0,
		"and an unattributed one is still treated as noise",
	);
});

test("adapter-2.x naming inside a gateway row still identifies this server", () => {
	// `alexandria_[a-z_]+` is the half of referencesAlexandria() that narrowing the regex
	// to /mcp__alexandria__/ silently deletes — verified: that mutation fails no test
	// without this fixture. It is also the broader half, since it matches this repo's own
	// crate names, so both directions are pinned.
	assert.equal(
		pair("mcp", 'Validation failed for tool "alexandria_store_memory":\n  - tags: must be either array or null')
			.length,
		1,
		"adapter 2.x names a real gateway shape",
	);
	assert.equal(
		pair("mcp", 'Validation failed for tool "newtopia_count": search_terms field is required').length,
		0,
		"another server's argument error stays noise",
	);
});

test("a class evicted by the ring can be stored on a later turn", () => {
	const mk = (n: string) => `build failed for ${n} crate: linker dropped a section, add the missing crate`;
	const t = new ErrorTracker();
	for (const n of ["a", "b", "c", "d", "e"]) t.recordError(`t${n}`, mk(n));
	t.recordError("tz", mk("z")); // evicts ta
	t.flush(); // turn boundary
	t.recordError("ta", mk("a"));
	t.recordSuccess("ta", "added the crate to Cargo.toml and the build passed");
	const out = t.flush();
	assert.equal(out.length, 1, "an evicted class must not be lost for the whole session");
	assert.match(out[0].content, /for a crate/);
});

test("indexed fields with different names are different classes", () => {
	const t = new ErrorTracker();
	t.recordError("bash", "surrealdb write failed: unknown field (tags[0]) in the memory payload");
	t.recordSuccess("bash", "dropped the field and the write went through");
	t.recordError("bash", "surrealdb write failed: unknown field (content[0]) in the memory payload");
	t.recordSuccess("bash", "dropped the field and the write went through");
	assert.equal(t.flush().length, 2, "eliding the field name merges two distinct causes");
});

test("a class that already produced a memory is not stored again next turn", () => {
	const E = "cargo test failed: 12 passed but the doctest for parse_memory errored on borrow";
	const t = new ErrorTracker();
	t.recordError("bash", E);
	t.recordSuccess("bash", "rewrote the doctest to clone before borrowing");
	assert.equal(t.flush().length, 1, "first turn stores it");
	t.recordError("bash", E);
	t.recordSuccess("bash", "rewrote the doctest to clone before borrowing");
	assert.equal(t.flush().length, 0, "the same class must stay deduped across turns");
});

test("whitespace-only differences share one class", () => {
	const t = new ErrorTracker();
	t.recordError("bash", "migrate failed: undefined field\n  session\n    on line 4 of the migration");
	t.recordSuccess("bash", "quoted the reserved word and it applied");
	t.recordError("bash", "migrate failed: undefined field session on line 4 of the migration");
	t.recordSuccess("bash", "quoted the reserved word and it applied");
	assert.equal(t.flush().length, 1, "class keys must be newline-insensitive");
});
