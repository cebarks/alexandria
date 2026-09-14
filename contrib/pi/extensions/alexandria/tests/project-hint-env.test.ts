/**
 * The ALEXANDRIA_REMINDERS_PROJECT override, in its own file on purpose.
 *
 * `src/config.ts` is a load-time singleton and `src/reminders.js` reads the value
 * it was built with, so "an env override wins" and "the git probe runs"
 * (reminders.test.ts) need two different first-import states. The test runner
 * gives every file its own process, which is the only seam that makes both
 * observable without reshaping src for the sake of tests.
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

// Deliberately padded: the server matches a target exactly, so an override that
// reached it with whitespace would silently degrade project delivery. Both cases
// below are arranged so a git probe could not produce this string.
process.env.ALEXANDRIA_CLIENT_CONFIG = absentClientToml;
process.env.ALEXANDRIA_REMINDERS_PROJECT = "  alexandria  ";

const { getProjectHint, __resetProjectHint } = await import(
	"../src/reminders.js"
);

const nonRepoDir = await makeScratchDir("alexandria-hint-env-not-a-repo-");

test("the override answers where a probe would find nothing", async (t) => {
	if (await gitToplevel(nonRepoDir)) {
		t.skip("the OS temp directory is inside a git repository");
		return;
	}
	__resetProjectHint();
	// Outside a repository the probe can only return undefined, so a trimmed
	// "alexandria" here can only have come from the environment.
	assert.equal(await inDir(nonRepoDir, () => getProjectHint()), "alexandria");
});

test("the override wins inside a repository as well", async (t) => {
	const repoRoot = await gitToplevel(extensionRoot);
	if (repoRoot === undefined) {
		t.skip("git is unavailable, so there is no repository to override");
		return;
	}
	if (basename(repoRoot) === "alexandria") {
		// A checkout named after the override would make this pass either way.
		t.skip("the repository root is named like the override");
		return;
	}
	__resetProjectHint();
	assert.equal(
		await inDir(join(repoRoot, "contrib"), () => getProjectHint()),
		"alexandria",
	);
});
