/**
 * The diagnostic log's contract: it must be bounded, it must survive an unwritable
 * path, and it must never report a stall that did not happen.
 *
 * `recordPromptPath` is fire-and-forget by design, so these tests poll for the write
 * rather than awaiting a handle — adding one just for tests would put a promise in
 * the prompt path that nobody is allowed to await.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after } from "node:test";

process.env.ALEXANDRIA_CLIENT_CONFIG ??= "/tmp/alexandria-tests-absent-client.toml";

const { logPath, recordPromptPath, outcomeLabel, startDriftProbe } = await import(
	"../src/diag.js"
);
type PromptPathSample = import("../src/diag.js").PromptPathSample;
const { AlexandriaFailure } = await import("../src/failure.js");

const sample = (over: Partial<PromptPathSample> = {}): PromptPathSample => ({
	t: "2026-09-22T20:00:00.000Z",
	ms: 47,
	cold: false,
	driftMs: 0,
	recall: "injected",
	reminders: "none (0 due)",
	...over,
});

/** Each test gets its own state dir, so rolling and unwritable-path cases cannot
 *  interfere with each other. */
async function withStateDir<T>(run: (dir: string) => Promise<T>): Promise<T> {
	const dir = await mkdtemp(join(tmpdir(), "alexandria-diag-"));
	const previous = process.env.XDG_STATE_HOME;
	process.env.XDG_STATE_HOME = dir;
	try {
		return await run(dir);
	} finally {
		if (previous === undefined) delete process.env.XDG_STATE_HOME;
		else process.env.XDG_STATE_HOME = previous;
		await rm(dir, { recursive: true, force: true });
	}
}

async function waitForFile(path: string, ms = 2000): Promise<string> {
	const deadline = Date.now() + ms;
	for (;;) {
		try {
			return await readFile(path, "utf8");
		} catch {
			if (Date.now() > deadline) throw new Error(`timed out waiting for ${path}`);
			await new Promise((r) => setTimeout(r, 20));
		}
	}
}

test("logPath honours XDG_STATE_HOME and falls back to ~/.local/state", async () => {
	await withStateDir(async (dir) => {
		assert.equal(logPath(), join(dir, "alexandria-companion", "prompt-path.jsonl"));
	});
	delete process.env.XDG_STATE_HOME;
	assert.match(logPath(), /\.local\/state\/alexandria-companion\/prompt-path\.jsonl$/);
});

test("a record lands as one JSON line, and the directory is created", async () => {
	await withStateDir(async () => {
		recordPromptPath(sample({ ms: 123, cold: true }));
		const text = await waitForFile(logPath());
		const lines = text.trim().split("\n");
		assert.equal(lines.length, 1);
		const parsed = JSON.parse(lines[0]) as PromptPathSample;
		assert.equal(parsed.ms, 123);
		assert.equal(parsed.cold, true);
		assert.equal(parsed.recall, "injected");
	});
});

test("the log rolls instead of growing without bound, keeping the newest", async () => {
	await withStateDir(async () => {
		const path = logPath();
		// Seed past the cap directly: appending 256KB one record at a time would make
		// this the slowest test in the suite for no additional coverage.
		const filler = Array.from({ length: 4000 }, (_, i) =>
			JSON.stringify(sample({ ms: i })),
		).join("\n");
		assert.ok(filler.length > 256 * 1024, "seed must exceed the cap");
		const { mkdir } = await import("node:fs/promises");
		const { dirname } = await import("node:path");
		await mkdir(dirname(path), { recursive: true });
		await writeFile(path, `${filler}\n`);

		recordPromptPath(sample({ ms: 999_999 }));

		const deadline = Date.now() + 3000;
		let text = "";
		for (;;) {
			text = await readFile(path, "utf8");
			if (text.includes("999999")) break;
			if (Date.now() > deadline) throw new Error("roll did not complete");
			await new Promise((r) => setTimeout(r, 20));
		}

		assert.ok(
			text.length < 256 * 1024,
			`rolled file should be under the cap, got ${text.length}`,
		);
		const lines = text.trim().split("\n");
		// Newest survives, oldest is gone, and every retained line is whole JSON —
		// a byte-offset slice would leave a truncated record at the head.
		assert.match(lines[lines.length - 1], /999999/);
		assert.ok(!text.includes('"ms":0,'), "the oldest records should have been dropped");
		for (const line of lines) assert.doesNotThrow(() => JSON.parse(line));
	});
});

test("an unwritable state dir is swallowed, not thrown into the prompt path", async () => {
	await withStateDir(async (dir) => {
		// A file where the directory should be makes mkdir and append fail.
		process.env.XDG_STATE_HOME = join(dir, "not-a-dir");
		const { writeFile: wf, mkdir } = await import("node:fs/promises");
		await mkdir(join(dir, "not-a-dir"), { recursive: true });
		await writeFile(join(dir, "not-a-dir", "alexandria-companion"), "blocked");
		assert.doesNotThrow(() => recordPromptPath(sample()));
		// Give the rejected promise a chance to surface as an unhandled rejection,
		// which would fail the run.
		await new Promise((r) => setTimeout(r, 100));
	});
});

// ── outcomeLabel ────────────────────────────────────────────────────────────────

const fulfilled = (value: { block: string | null; error?: string; count?: number }) =>
	({ status: "fulfilled" as const, value });
const rejected = (reason: unknown) => ({ status: "rejected" as const, reason });

test("outcomeLabel names an injection, an empty result, and a payload error", () => {
	assert.equal(outcomeLabel(fulfilled({ block: "B" })), "injected");
	assert.equal(outcomeLabel(fulfilled({ block: null })), "none");
	assert.equal(outcomeLabel(fulfilled({ block: null, count: 0 })), "none (0 due)");
	assert.equal(outcomeLabel(fulfilled({ block: null, count: 2 })), "none (2 due)");
	assert.equal(
		outcomeLabel(fulfilled({ block: null, error: "malformed response" })),
		"payload: malformed response",
	);
});

test("outcomeLabel carries the classification a rejection was given", () => {
	const cancelled = rejected(
		new AlexandriaFailure("This operation was aborted", {
			kind: "cancelled",
			resetConnection: false,
		}),
	);
	assert.equal(outcomeLabel(cancelled), "cancelled: This operation was aborted");

	const transport = rejected(
		Object.assign(new TypeError("fetch failed"), {
			cause: Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:3000"), {
				code: "ECONNREFUSED",
			}),
		}),
	);
	assert.match(outcomeLabel(transport), /^transport: /);
	assert.match(outcomeLabel(transport), /ECONNREFUSED/);
});

// ── drift probe ─────────────────────────────────────────────────────────────────

test("the drift probe reports ~0 when the loop is responsive", async () => {
	const stop = startDriftProbe(20);
	await new Promise((r) => setTimeout(r, 120));
	const drift = stop();
	assert.ok(drift < 50, `expected a responsive loop, got driftMs=${drift}`);
});

test("the drift probe reports a blocked loop, which is the whole point", async () => {
	const stop = startDriftProbe(20);
	// A synchronous block is exactly what another extension's execSync does.
	const sab = new SharedArrayBuffer(4);
	Atomics.wait(new Int32Array(sab), 0, 0, 400);
	const drift = stop();
	assert.ok(drift >= 300, `a 400ms block must be reported, got driftMs=${drift}`);
});

test("stopping the probe twice is harmless", () => {
	const stop = startDriftProbe(20);
	stop();
	assert.equal(typeof stop(), "number");
});
