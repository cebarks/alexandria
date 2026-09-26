/**
 * Prompt-path diagnostics: a small rolling JSONL log, and a probe for event-loop
 * stalls.
 *
 * This exists because the failure being investigated cannot be reproduced on
 * demand. A healthy server (p50 45 ms, p99 116 ms) intermittently gets reported as
 * `REQUEST_TIMEOUT`, and the only mechanism that survives measurement is a stall of
 * pi's main loop overlapping an in-flight MCP handshake — reproducible 6/6 when
 * forced, but never yet observed in the field. Without a record of what actually
 * happened, every future occurrence is another guessing session.
 *
 * **Nothing here may affect the path it measures.** Writes are async and
 * fire-and-forget, every entry point swallows its own errors, the probe timer is
 * `unref`'d, and no call is ever awaited by the prompt path. A diagnostic that can
 * block the event loop would be worse than none, because a blocked loop is precisely
 * the suspected cause.
 */

import { appendFile, mkdir, readFile, stat, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { failureOf } from "./failure.js";

/** One prompt-path run, as a single line. Field names are short because this file
 *  is written on every prompt and read by a human with `tail`. */
export interface PromptPathSample {
	/** ISO-8601, so entries can be lined up against the server's journal. */
	t: string;
	/** Whole handler duration, including the git probe and any stall. */
	ms: number;
	/** True when this run established the MCP connection. The cold handshake is the
	 *  only window measured to lose the race against a stalled loop, so this is the
	 *  field that distinguishes the interesting rows. */
	cold: boolean;
	/** Worst event-loop lateness observed during the run, in ms. `driftMs` above a
	 *  few hundred means something else in pi's process blocked the loop — which is
	 *  the difference between "the server was slow" and "the server was fine". */
	driftMs: number;
	recall: string;
	reminders: string;
}

/** Cap before rolling. Sized for a few days of prompts, not for archival. */
const MAX_BYTES = 256 * 1024;

/** Roll down to this fraction, so trimming is not a per-line operation. */
const KEEP_FRACTION = 0.5;

function stateDir(): string {
	// Read at call time, not at import, so tests can point it at a temp dir without
	// module mocking — which hangs under tsx.
	const base = process.env.XDG_STATE_HOME?.trim();
	return base ? base : join(homedir(), ".local", "state");
}

/** Where the rolling log lives. `$XDG_STATE_HOME/alexandria-companion/prompt-path.jsonl`. */
export function logPath(): string {
	return join(stateDir(), "alexandria-companion", "prompt-path.jsonl");
}

async function roll(path: string): Promise<void> {
	let size: number;
	try {
		size = (await stat(path)).size;
	} catch {
		return; // no file yet: nothing to roll
	}
	if (size <= MAX_BYTES) return;
	// Keep the most recent half by bytes. Splitting on newlines rather than slicing
	// at an offset, so the file never starts with a truncated record.
	const text = await readFile(path, "utf8");
	const lines = text.split("\n").filter((l) => l !== "");
	const budget = MAX_BYTES * KEEP_FRACTION;
	const kept: string[] = [];
	let total = 0;
	for (let i = lines.length - 1; i >= 0 && total < budget; i--) {
		kept.unshift(lines[i]);
		total += lines[i].length + 1;
	}
	await writeFile(path, kept.length > 0 ? `${kept.join("\n")}\n` : "");
}

async function append(sample: PromptPathSample): Promise<void> {
	const path = logPath();
	await mkdir(dirname(path), { recursive: true });
	await roll(path);
	await appendFile(path, `${JSON.stringify(sample)}\n`);
}

/**
 * Record one prompt-path run. Fire-and-forget by design: the caller must not await
 * this, and a failure to write is silently dropped — losing a diagnostic line is
 * acceptable, delaying or breaking a prompt is not.
 */
export function recordPromptPath(sample: PromptPathSample): void {
	void append(sample).catch(() => {
		/* diagnostics never surface their own failures */
	});
}

/**
 * Render one settled feature as a short label for the log.
 *
 * Pure and exported for the same reason as everything else here: the handler that
 * would otherwise contain this logic is an inline closure that cannot be reached
 * from a test without module mocking.
 */
export function outcomeLabel(
	settled: PromiseSettledResult<{ block: string | null; error?: string; count?: number }>,
): string {
	if (settled.status === "rejected") {
		const f = failureOf(settled.reason);
		return `${f.kind}: ${f.cause}`;
	}
	if (settled.value.error) return `payload: ${settled.value.error}`;
	if (settled.value.block) return "injected";
	// Distinguish "asked and nothing matched" from "never asked" (feature off):
	// the reminder path always reports a count, recall does not.
	return settled.value.count !== undefined ? `none (${settled.value.count} due)` : "none";
}

/** Stop function returned by {@linkcode startDriftProbe}: ends the probe and reports
 *  the worst lateness seen. */
export type DriftProbe = () => number;

/**
 * Measure how late a repeating timer fires, which is how long the event loop was
 * blocked by *anything* in pi's process — another extension's `execSync`, a large
 * synchronous parse, pi's own session-file I/O.
 *
 * A timer cannot measure the loop it runs on from inside, so this reports lateness
 * after the fact: if a 100 ms interval ticks 6 s late, something held the loop for
 * ~6 s. That is the datum which distinguishes a slow server from a stalled client,
 * and it is why the probe belongs in the log rather than in a warning.
 *
 * `unref`'d so a diagnostic can never hold the process open.
 *
 * Lateness is also measured at stop time, not only on ticks. A block that ends
 * immediately before the caller stops the probe leaves the overdue callback no
 * chance to run, so relying on ticks alone would report zero drift for exactly the
 * stall it exists to catch.
 */
export function startDriftProbe(intervalMs = 100): DriftProbe {
	let maxLate = 0;
	let expected = Date.now() + intervalMs;
	const timer = setInterval(() => {
		const now = Date.now();
		const late = now - expected;
		if (late > maxLate) maxLate = late;
		expected = now + intervalMs;
	}, intervalMs);
	timer.unref?.();
	return () => {
		clearInterval(timer);
		const late = Date.now() - expected;
		if (late > maxLate) maxLate = late;
		return Math.max(0, maxLate);
	};
}
