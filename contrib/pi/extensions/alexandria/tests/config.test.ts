/**
 * Config precedence smoke tests for the pi companion.
 *
 * `CONFIG` is built while `src/config.ts` is evaluated, so a case cannot change
 * its input and re-read it: each one re-imports the module under a unique query
 * string (tsx instantiates a separate module per distinct specifier) after
 * rewriting the environment that load should see.
 *
 * `ALEXANDRIA_CLIENT_CONFIG` is always pointed into this file's temp directory —
 * at a fixture, or at a name that is never created — so a developer's own
 * `~/.config/alexandria/client.toml` can never decide an expectation, and no case
 * reads a real config file.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import { makeScratchDir } from "./helpers.js";

const CONFIG_MODULE = new URL("../src/config.ts", import.meta.url);

/** Every variable `src/config.ts` reads, so no case inherits state from the shell. */
const MANAGED_ENV = [
	"ALEXANDRIA_URL",
	"ALEXANDRIA_AUTO_RECALL",
	"ALEXANDRIA_AUTO_RECALL_LIMIT",
	"ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY",
	"ALEXANDRIA_AUTO_STORE",
	"ALEXANDRIA_EXTRACT_MODEL",
	"ALEXANDRIA_EXTRACT_TIMEOUT_MS",
	"ALEXANDRIA_REMINDERS",
	"ALEXANDRIA_REMINDERS_PROJECT",
	"ALEXANDRIA_CLIENT_CONFIG",
];

/** A file that turns every knob off or to a non-default value, including a
 *  whitespace-padded project, so "the file said it" is distinguishable from
 *  "the default said it" for each field. */
const FULL_TOML = [
	"[server]",
	'url = "http://127.0.0.1:9999/mcp"',
	"",
	"[recall]",
	"enabled = false",
	"limit = 7",
	"min_similarity = 0.42",
	"",
	"[store]",
	"enabled = false",
	'extract_model = "test/extract-model"',
	"extract_timeout_ms = 1234",
	"",
	"[reminders]",
	"enabled = false",
	'project = "  from-toml  "',
].join("\n");

const workspace = await makeScratchDir("alexandria-client-config-");
/** A path that is never created: the "no client.toml at all" case. */
const absentToml = join(workspace, "absent.toml");
const fullToml = join(workspace, "full.toml");
const brokenToml = join(workspace, "broken.toml");
await Promise.all([
	writeFile(fullToml, FULL_TOML, "utf8"),
	writeFile(brokenToml, "this is not = valid toml [[[", "utf8"),
]);

let instances = 0;

/**
 * `CONFIG` as a freshly started extension would see it with exactly `env` set.
 *
 * The queried specifier is the cache bust, and TypeScript cannot resolve a
 * specifier carrying one, so the cast re-attaches the real module type —
 * `CONFIG`'s fields stay type-checked here and at every `src/` call site.
 */
async function configWith(
	env: Record<string, string> = {},
): Promise<typeof import("../src/config.js").CONFIG> {
	const previous = new Map(
		MANAGED_ENV.map((key) => [key, process.env[key]] as const),
	);
	try {
		for (const key of MANAGED_ENV) delete process.env[key];
		for (const [key, value] of Object.entries(env)) process.env[key] = value;
		const loaded = (await import(
			`${CONFIG_MODULE.href}?case=${instances++}`
		)) as typeof import("../src/config.js");
		return loaded.CONFIG;
	} finally {
		// Leave no trace: a leaked variable would mislead every later case in this
		// file, and the runner starts from the developer's own shell environment.
		for (const [key, value] of previous) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	}
}

test("no env and no file: every feature is on, and the defaults hold", async () => {
	const config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: absentToml });
	assert.equal(config.serverUrl, "http://127.0.0.1:3000/mcp");
	assert.equal(config.recallDisabled, false);
	assert.equal(config.storeDisabled, false);
	assert.equal(config.remindersDisabled, false);
	assert.equal(config.recallLimit, 5);
	assert.equal(config.recallMinSimilarity, 0.58);
	assert.equal(config.extractModel, "vertex/claude-haiku-4-5");
	assert.equal(config.extractTimeoutMs, 5000);
	assert.equal(config.remindersProject, undefined);
});

test("legacy ALEXANDRIA_AUTO_RECALL=off disables recall alone", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentToml,
		ALEXANDRIA_AUTO_RECALL: "off",
	});
	assert.equal(config.recallDisabled, true);
	assert.equal(config.storeDisabled, false);
	assert.equal(config.remindersDisabled, false);
});

test("ALEXANDRIA_AUTO_STORE=off disables store alone", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentToml,
		ALEXANDRIA_AUTO_STORE: "off",
	});
	assert.equal(config.storeDisabled, true);
	assert.equal(config.recallDisabled, false);
	assert.equal(config.remindersDisabled, false);
});

test("ALEXANDRIA_REMINDERS=off disables reminders alone", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentToml,
		ALEXANDRIA_REMINDERS: "off",
	});
	assert.equal(config.remindersDisabled, true);
	assert.equal(config.recallDisabled, false);
	assert.equal(config.storeDisabled, false);
});

test("client.toml drives every field when no env is set", async () => {
	const config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: fullToml });
	assert.equal(config.serverUrl, "http://127.0.0.1:9999/mcp");
	assert.equal(config.recallDisabled, true);
	assert.equal(config.storeDisabled, true);
	assert.equal(config.remindersDisabled, true);
	assert.equal(config.recallLimit, 7);
	assert.equal(config.recallMinSimilarity, 0.42);
	assert.equal(config.extractModel, "test/extract-model");
	assert.equal(config.extractTimeoutMs, 1234);
	// The target the server matches is trimmed on the way in, not sent padded.
	assert.equal(config.remindersProject, "from-toml");
});

test("explicit env beats client.toml in both directions", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullToml,
		ALEXANDRIA_URL: "http://127.0.0.1:4000/mcp",
		// Not "off": any set value counts as an explicit choice, so the file's
		// `enabled = false` cannot leave a feature dead after the user opts back in.
		ALEXANDRIA_AUTO_RECALL: "on",
		ALEXANDRIA_AUTO_STORE: "on",
		ALEXANDRIA_REMINDERS: "on",
		ALEXANDRIA_AUTO_RECALL_LIMIT: "3",
		ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY: "0.1",
		ALEXANDRIA_EXTRACT_MODEL: "env/model",
		ALEXANDRIA_EXTRACT_TIMEOUT_MS: "4321",
	});
	assert.equal(config.serverUrl, "http://127.0.0.1:4000/mcp");
	assert.equal(config.recallDisabled, false);
	assert.equal(config.storeDisabled, false);
	assert.equal(config.remindersDisabled, false);
	assert.equal(config.recallLimit, 3);
	assert.equal(config.recallMinSimilarity, 0.1);
	assert.equal(config.extractModel, "env/model");
	assert.equal(config.extractTimeoutMs, 4321);
});

test("ALEXANDRIA_REMINDERS_PROJECT beats client.toml and arrives trimmed", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullToml,
		ALEXANDRIA_REMINDERS_PROJECT: "  alexandria  ",
	});
	assert.equal(config.remindersProject, "alexandria");
});

test("a whitespace-only ALEXANDRIA_REMINDERS_PROJECT falls through to the file", async () => {
	// A set-but-blank override must not be sent verbatim: the server matches the
	// target exactly, so a padded or empty project silently degrades delivery to
	// escalation-only. Falling through to the file keeps delivery working.
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullToml,
		ALEXANDRIA_REMINDERS_PROJECT: "   ",
	});
	assert.equal(config.remindersProject, "from-toml");
});

test("an unparseable client.toml falls back to defaults instead of throwing", async () => {
	// config.ts warns about the path it could not read — that line on stdout is
	// expected. What the prompt path depends on is a usable CONFIG either way.
	const config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: brokenToml });
	assert.equal(config.remindersDisabled, false);
	assert.equal(config.recallDisabled, false);
	assert.equal(config.recallLimit, 5);
	assert.equal(config.remindersProject, undefined);
});

test("a set-but-blank toggle does not re-enable what client.toml disabled", async () => {
	// `ALEXANDRIA_REMINDERS=` in a direnv or .env is not a user saying "off", and
	// gating the file branch on `process.env.X === undefined` let a blank value win
	// the precedence race by being *present* — silently putting the consuming
	// per-prompt check_reminders call back on with no signal.
	for (const [key, field] of [
		["ALEXANDRIA_AUTO_RECALL", "recallDisabled"],
		["ALEXANDRIA_AUTO_STORE", "storeDisabled"],
		["ALEXANDRIA_REMINDERS", "remindersDisabled"],
	] as const) {
		for (const blank of ["", "   "]) {
			const config = await configWith({
				ALEXANDRIA_CLIENT_CONFIG: fullToml,
				[key]: blank,
			});
			assert.equal(
				config[field as "recallDisabled" | "storeDisabled" | "remindersDisabled"],
				true,
				`${key}=${JSON.stringify(blank)} overrode the file's enabled=false`,
			);
		}
	}
});

test("an explicit env value still overrides the file in both directions", async () => {
	// The blank cases above must not have quietly broken the real contract.
	const off = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentToml,
		ALEXANDRIA_REMINDERS: "off",
	});
	assert.equal(off.remindersDisabled, true);
	const padded = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullToml,
		ALEXANDRIA_REMINDERS: "  off  ",
	});
	assert.equal(padded.remindersDisabled, true, "a padded ' off ' means off");
});

test("a blank or unparseable numeric override falls back instead of becoming 0 or NaN", async () => {
	// `Number("")` is 0 and `Number("ten")` is NaN: a blank limit silenced recall
	// entirely, and `similarity >= NaN` is false for every row, so recall looked
	// like an empty database rather than a broken config.
	for (const key of [
		"ALEXANDRIA_AUTO_RECALL_LIMIT",
		"ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY",
		"ALEXANDRIA_EXTRACT_TIMEOUT_MS",
	]) {
		for (const junk of ["", "  ", "ten", "1e999"]) {
			const config = await configWith({
				ALEXANDRIA_CLIENT_CONFIG: absentToml,
				[key]: junk,
			});
			const value =
				key === "ALEXANDRIA_AUTO_RECALL_LIMIT"
					? config.recallLimit
					: key === "ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY"
						? config.recallMinSimilarity
						: config.extractTimeoutMs;
			assert.ok(Number.isFinite(value), `${key}=${JSON.stringify(junk)} -> ${value}`);
			assert.ok(value > 0, `${key}=${JSON.stringify(junk)} -> ${value}`);
		}
	}
	// A real number still wins over the file, and the file still wins over defaults.
	const numeric = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullToml,
		ALEXANDRIA_AUTO_RECALL_LIMIT: "3",
	});
	assert.equal(numeric.recallLimit, 3);
	const fromFile = await configWith({ ALEXANDRIA_CLIENT_CONFIG: fullToml });
	assert.equal(fromFile.recallLimit, 7);
});
