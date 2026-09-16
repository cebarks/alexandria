/**
 * Reminders smoke tests for the pi companion: the block the agent is shown
 * (`formatDueBlock`) and the delivery-target probe (`getProjectHint`,
 * `__resetProjectHint`).
 *
 * Pure functions and cwd/env manipulation only — nothing here opens a socket or
 * talks to a server. `checkReminders` is intentionally not covered: reaching it
 * without a server means stubbing `./mcp-client.js`, which under tsx needs
 * experimental module mocking (it hung the loader instead of isolating the call).
 * The dispatcher that consumes its result was probe-verified in Task 14.
 *
 * The module graph is imported dynamically rather than statically: `src/config.ts`
 * reads `process.env` while it is being evaluated, and the probe cases below need
 * the no-override state, so that baseline has to be applied first.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { basename, join } from "node:path";
import {
	absentClientToml,
	extensionRoot,
	gitToplevel,
	inDir,
	makeScratchDir,
} from "./helpers.js";

// Hermetic baseline for the config singleton this module pulls in: no project
// override from the shell, and no real client.toml on disk to supply one.
process.env.ALEXANDRIA_CLIENT_CONFIG = absentClientToml;
delete process.env.ALEXANDRIA_REMINDERS_PROJECT;

const { formatDueBlock, getProjectHint, __resetProjectHint } = await import(
	"../src/reminders.js"
);
type DueReminder = import("../src/reminders.js").DueReminder;

const HEADER =
	/^⏰ Due reminders from Alexandria \(each of these is consumed now and will not be repeated unless it is marked recurring/;

/** Render one reminder the way a delivery would. */
function bullet(item: DueReminder): string {
	const rendered = formatDueBlock([item]).split("\n");
	assert.equal(rendered.length, 2, "a header plus exactly one bullet");
	return rendered[1];
}

// ── formatDueBlock ──────────────────────────────────────────────────────

test("the block is a header plus one bullet per reminder, in input order", () => {
	const rendered = formatDueBlock([
		{
			id: "reminder:1",
			message: "renew the TLS cert",
			target: "project:infra",
			escalated: true,
			missed_occurrences: 2,
			due_at: "2026-09-12T09:00:00Z",
		},
		{
			id: "reminder:2",
			message: "post the standup note",
			target: "global",
			recurring: true,
			next_due_at: "2026-09-18T14:00:00Z",
			schedule: "every Friday at 09:00",
			note: "#standup channel",
			provenance: { project: "infra", session_id: "01JABC" },
		},
	]).split("\n");

	assert.equal(rendered.length, 3);
	// The header is the instruction the agent reads on every delivery: it has to
	// say that these rows are consumed, and that recurring ones come again.
	assert.match(rendered[0], HEADER);
	assert.match(rendered[0], /act on it or tell the user/);
	assert.deepEqual(rendered.slice(1), [
		"- renew the TLS cert [id reminder:1] [target project:infra] [OVERDUE — escalated from project targeting] (missed 2 earlier occurrence(s)) (due 2026-09-12T09:00:00Z UTC)",
		"- post the standup note [id reminder:2] (every Friday at 09:00) (⟲ recurring — next fire 2026-09-18T14:00:00Z UTC) note: #standup channel [set in infra, session 01JABC]",
	]);
});

test("an empty delivery still renders the header", () => {
	assert.match(formatDueBlock([]), /⏰ Due reminders/);
});

test("a reminder with nothing else set renders only its text", () => {
	// The most common row, and the one where an absent optional field could leak
	// "undefined" into the context the model is asked to read.
	assert.equal(
		bullet({ id: "reminder:3", message: "ping the on-call" }),
		"- ping the on-call [id reminder:3]",
	);
});

test("project targets are named, global targets are not", () => {
	assert.equal(
		bullet({
			id: "reminder:4",
			message: "rotate keys",
			target: "project:infra",
		}),
		"- rotate keys [id reminder:4] [target project:infra]",
	);
	// "global" was the sender's routing decision, not information for the agent:
	// printing it would invite the model to reason about a project that none of
	// these rows are aimed at.
	assert.equal(
		bullet({ id: "reminder:5", message: "file taxes", target: "global" }),
		"- file taxes [id reminder:5]",
	);
});

test("escalation and coalesced occurrences are marked", () => {
	const escalated = bullet({
		id: "reminder:6",
		message: "renew cert",
		escalated: true,
	});
	assert.match(escalated, /\[OVERDUE — escalated from project targeting\]/);

	const missed = bullet({
		id: "reminder:7",
		message: "standup",
		missed_occurrences: 3,
	});
	assert.match(missed, /\(missed 3 earlier occurrence\(s\)\)/);

	// A zero count means "nothing was skipped": not a fact worth a line.
	assert.doesNotMatch(
		bullet({ id: "reminder:8", message: "a", missed_occurrences: 0 }),
		/missed/,
	);
	assert.doesNotMatch(
		bullet({ id: "reminder:9", message: "a", escalated: false }),
		/OVERDUE/,
	);
});

test("a recurring row says either its next fire or that this one is final", () => {
	assert.equal(
		bullet({
			id: "reminder:10",
			message: "standup",
			recurring: true,
			next_due_at: "2026-09-19T09:00:00Z",
		}),
		"- standup [id reminder:10] (⟲ recurring — next fire 2026-09-19T09:00:00Z UTC)",
	);
	// A null next_due_at means the schedule is finished (a one-shot, or no fire
	// left), and the agent must not promise the user another one.
	assert.equal(
		bullet({
			id: "reminder:11",
			message: "standup",
			recurring: true,
			next_due_at: null,
		}),
		"- standup [id reminder:11] (⟲ recurring — final fire, nothing scheduled after this)",
	);
	assert.equal(
		bullet({ id: "reminder:12", message: "standup", recurring: true }),
		"- standup [id reminder:12] (⟲ recurring — final fire, nothing scheduled after this)",
		"an absent next_due_at reads as final, not as a blank timestamp",
	);
	assert.doesNotMatch(
		bullet({
			id: "reminder:13",
			message: "standup",
			next_due_at: "2026-09-19T09:00:00Z",
		}),
		/⟲/,
		"a one-shot is not marked recurring",
	);
});

test("due instant and human-readable schedule both render, instant first", () => {
	assert.equal(
		bullet({
			id: "reminder:14",
			message: "deploy",
			due_at: "2026-09-12T09:00:00Z",
			schedule: "every weekday at 17:30",
		}),
		"- deploy [id reminder:14] (due 2026-09-12T09:00:00Z UTC) (every weekday at 17:30)",
	);
});

test("an empty, blank or absent message renders a placeholder", () => {
	assert.equal(
		bullet({ id: "reminder:15", message: "" }),
		"- (reminder with no text) [id reminder:15]",
	);
	assert.equal(
		bullet({ id: "reminder:16", message: " \n  " }),
		"- (reminder with no text) [id reminder:16]",
	);
	// Rows arrive out of JSON, so a missing key is a real possibility.
	assert.equal(
		bullet({ id: "reminder:17" } as unknown as DueReminder),
		"- (reminder with no text) [id reminder:17]",
	);
});

test("provenance says where the reminder was set", () => {
	assert.equal(
		bullet({
			id: "reminder:18",
			message: "a",
			provenance: { project: "api", session_id: "s1" },
		}),
		"- a [id reminder:18] [set in api, session s1]",
	);
	assert.equal(
		bullet({ id: "reminder:19", message: "a", provenance: { project: "api" } }),
		"- a [id reminder:19] [set in api]",
	);
	assert.equal(
		bullet({ id: "reminder:20", message: "a", provenance: { session_id: "s1" } }),
		"- a [id reminder:20] [set in unknown project, session s1]",
	);
	// Nothing known about the origin is nothing to say.
	assert.doesNotMatch(
		bullet({ id: "reminder:21", message: "a", provenance: {} }),
		/\[set in/,
	);
});

test("newlines anywhere in a reminder collapse into its single line", () => {
	// A multi-line message would read as a continuation of the previous bullet, so
	// the sanitizer covers every interpolated field, not just the text.
	assert.equal(
		bullet({
			id: "reminder:22",
			message: "renew\n\tthe\ncert",
			target: "project:two\nlines",
			due_at: "2026-09-12T09:00:00Z",
			schedule: "every\nFriday",
			recurring: true,
			next_due_at: "2026-09-13T09:00:00Z",
			note: "first\nsecond",
			provenance: { project: "origin\nrepo", session_id: "abc\ndef" },
		}),
		"- renew the cert [id reminder:22] [target project:two lines] (due 2026-09-12T09:00:00Z UTC) (every Friday) (⟲ recurring — next fire 2026-09-13T09:00:00Z UTC) note: first second [set in origin repo, session abc def]",
	);
});

// ── getProjectHint ──────────────────────────────────────────────────────
//
// These three cases share one process and change `process.cwd()`, so they must
// stay sequential (node:test runs root tests in order) and must not be given
// `concurrency`. The ALEXANDRIA_REMINDERS_PROJECT override is exercised in
// project-hint-env.test.ts instead, because the config singleton carrying it is
// built once per process and this file needs it empty.

/** The repo the tests live in, or undefined when git cannot be used. */
const repoRoot = await gitToplevel(extensionRoot);
/** A scratch dir, unless the OS temp dir is itself inside a repository. */
const nonRepoDir = await makeScratchDir("alexandria-hint-not-a-repo-");

test("getProjectHint names the repo root, not the directory the session runs in", async (t) => {
	if (repoRoot === undefined) {
		t.skip("git is unavailable, so the probe cannot be exercised");
		return;
	}
	__resetProjectHint();
	// contrib is a subdirectory whose own name is never the project's, so a hint
	// taken from the cwd instead of the toplevel cannot pass this.
	assert.equal(
		await inDir(join(repoRoot, "contrib"), () => getProjectHint()),
		basename(repoRoot),
	);
});

test("getProjectHint fails open to no hint outside a git repository", async (t) => {
	if (await gitToplevel(nonRepoDir)) {
		t.skip("the OS temp directory is inside a git repository");
		return;
	}
	__resetProjectHint();
	assert.equal(await inDir(nonRepoDir, () => getProjectHint()), undefined);
});

test("a settled probe is cached per directory, not per process", async (t) => {
	if (repoRoot === undefined || (await gitToplevel(nonRepoDir))) {
		t.skip("needs git, and a temp directory outside any repository");
		return;
	}
	__resetProjectHint();
	// The miss is cached — for *that* directory. With one process-wide slot it used
	// to answer for every later directory, so a single prompt started outside a repo
	// pinned the session to no hint for the life of the process, and one process can
	// serve several projects across /resume.
	assert.equal(await getProjectHint(nonRepoDir), undefined);
	assert.equal(
		await getProjectHint(join(repoRoot, "contrib")),
		basename(repoRoot),
		"a directory inside a repository must still find its own hint",
	);
	// Repeats are served from the cache: that is what saves the git call.
	assert.equal(await getProjectHint(nonRepoDir), undefined);
	__resetProjectHint();
	assert.equal(
		await getProjectHint(join(repoRoot, "contrib")),
		basename(repoRoot),
		"after a reset the probe runs again",
	);
});

test("the session directory decides the hint, not the process cwd", async (t) => {
	if (repoRoot === undefined) {
		t.skip("git is unavailable, so the probe cannot be exercised");
		return;
	}
	__resetProjectHint();
	// A resumed session can live in a different project than the one pi was launched
	// from, and the server matches the target byte-exactly. Answering from
	// process.cwd() delivers the launching project's reminders to this session and
	// holds this session's own project reminders until they arrive escalated.
	const inRepo = join(repoRoot, "contrib");
	assert.equal(await getProjectHint(inRepo), basename(repoRoot));
	assert.equal(
		await inDir(extensionRoot, () => getProjectHint(inRepo)),
		basename(repoRoot),
		"an explicit session directory wins over the process cwd",
	);
});