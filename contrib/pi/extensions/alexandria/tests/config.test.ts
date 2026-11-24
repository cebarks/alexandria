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
 * `~/.config/alexandria/client.json` can never decide an expectation, and no case
 * reads a real config file.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { chmod, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { makeScratchDir } from "./helpers.js";

const CONFIG_MODULE = new URL("../src/config.ts", import.meta.url);

/** Every variable `src/config.ts` reads, so no case inherits state from the shell. */
const MANAGED_ENV = [
	"ALEXANDRIA_URL",
	"ALEXANDRIA_API_TOKEN",
	"ALEXANDRIA_PROJECT_ID",
	"ALEXANDRIA_AUTO_RECALL",
	"ALEXANDRIA_AUTO_RECALL_LIMIT",
	"ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY",
	"ALEXANDRIA_AUTO_STORE",
	"ALEXANDRIA_EXTRACT_MODEL",
	"ALEXANDRIA_EXTRACT_TIMEOUT_MS",
	"ALEXANDRIA_REMINDERS",
	"ALEXANDRIA_REMINDERS_PROJECT",
	"ALEXANDRIA_CLIENT_CONFIG",
	"ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS",
	"ALEXANDRIA_PROMPT_BUDGET_MS",
	"ANTHROPIC_API_KEY",
	"XDG_CONFIG_HOME",
];

/** A file that turns every knob off or to a non-default value, including a
 *  whitespace-padded project, so "the file said it" is distinguishable from
 *  "the default said it" for each field. */
const FULL_JSON = JSON.stringify({
	server: { url: "http://127.0.0.1:9999/mcp", api_token: "ignored-fixture-token", project_id: "project:ignored" },
	recall: { enabled: false, limit: 7, min_similarity: 0.42 },
	store: { enabled: false, extract_model: "test/extract-model", extract_timeout_ms: 1234 },
	reminders: { enabled: false, project: "  from-json  " },
});

const workspace = await makeScratchDir("alexandria-client-config-");
/** A path that is never created: the "no client.json at all" case. */
const absentJson = join(workspace, "absent.json");
const fullJson = join(workspace, "full.json");
const brokenJson = join(workspace, "broken.json");
await Promise.all([
	writeFile(fullJson, FULL_JSON, "utf8"),
	writeFile(brokenJson, "this is not valid JSON [[[", "utf8"),
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
		return { ...loaded.CONFIG } as typeof loaded.CONFIG;
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
	const config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: absentJson });
	assert.equal(config.serverUrl, "http://127.0.0.1:3000/mcp");
	assert.equal(config.apiToken, undefined);
	assert.equal(config.projectId, undefined);
	assert.equal(config.recallDisabled, false);
	assert.equal(config.remindersDisabled, false);
	assert.equal(config.recallLimit, 10);
	assert.equal(config.recallMinSimilarity, 0.45);
	assert.equal(config.remindersProject, undefined);
	assert.equal(config.promptCallTimeoutMs, 5000);
	assert.equal(config.promptBudgetMs, 10000);
});

test("transport timeouts load from client.json and environment overrides", async () => {
	const transportJson = join(workspace, "transport.json");
	await writeFile(
		transportJson,
		JSON.stringify({ transport: { prompt_call_timeout_ms: 1200, prompt_budget_ms: 4000 } }),
		"utf8",
	);
	const fromFile = await configWith({ ALEXANDRIA_CLIENT_CONFIG: transportJson });
	assert.equal(fromFile.promptCallTimeoutMs, 1200);
	assert.equal(fromFile.promptBudgetMs, 4000);
	const fromEnv = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: transportJson,
		ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS: "2500",
		ALEXANDRIA_PROMPT_BUDGET_MS: "8000",
	});
	assert.equal(fromEnv.promptCallTimeoutMs, 2500);
	assert.equal(fromEnv.promptBudgetMs, 8000);
});

test("transport timeout configuration rejects invalid bounds instead of clamping", async () => {
	const invalidConfigs: Record<string, string>[] = [
		{ ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS: "0" },
		{ ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS: "Infinity" },
		{ ALEXANDRIA_PROMPT_BUDGET_MS: "120001" },
		{ ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS: "6000", ALEXANDRIA_PROMPT_BUDGET_MS: "5000" },
	];
	for (const env of invalidConfigs) {
		await assert.rejects(
			configWith({ ALEXANDRIA_CLIENT_CONFIG: absentJson, ...env }),
			/transport.*(positive|finite|120000|less than or equal)/i,
		);
	}
});

test("default config path selects client.json instead of legacy TOML", async () => {
	const base = join(workspace, "xdg-default");
	await mkdir(join(base, "alexandria"), { recursive: true });
	await writeFile(join(base, "alexandria", "client.toml"), "[recall]\nlimit = 6\n");
	const withoutJson = await configWith({ XDG_CONFIG_HOME: base });
	assert.equal(withoutJson.recallLimit, 10);
	await writeFile(join(base, "alexandria", "client.json"), '{"recall":{"limit":19}}');
	const withJson = await configWith({ XDG_CONFIG_HOME: base });
	assert.equal(withJson.recallLimit, 19);
});

test("a blank client config path uses the default JSON location", async () => {
	const base = join(workspace, "blank-path-xdg");
	await mkdir(join(base, "alexandria"), { recursive: true });
	await writeFile(join(base, "alexandria", "client.json"), '{"recall":{"limit":12}}');
	const config = await configWith({ XDG_CONFIG_HOME: base, ALEXANDRIA_CLIENT_CONFIG: "  " });
	assert.equal(config.recallLimit, 12);
});

test("a nonblank client config path preserves significant whitespace", async () => {
	const path = join(workspace, "client with spaces ");
	await writeFile(path, '{"recall":{"limit":13}}');
	const config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: path });
	assert.equal(config.recallLimit, 13);
});

test("legacy ALEXANDRIA_AUTO_RECALL=off disables recall alone", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentJson,
		ALEXANDRIA_AUTO_RECALL: "off",
	});
	assert.equal(config.recallDisabled, true);
	assert.equal(config.remindersDisabled, false);
});

test("obsolete extraction environment settings are ignored with upgrade guidance", async () => {
	const warnings: string[] = [];
	const originalWarn = console.warn;
	console.warn = (message?: unknown) => warnings.push(String(message));
	try {
		const config = await configWith({
			ALEXANDRIA_CLIENT_CONFIG: absentJson,
			ALEXANDRIA_AUTO_STORE: "off",
			ALEXANDRIA_EXTRACT_MODEL: "legacy/model",
			ALEXANDRIA_EXTRACT_TIMEOUT_MS: "1234",
		});
		assert.equal(config.recallDisabled, false);
		assert.equal(config.remindersDisabled, false);
		assert.doesNotMatch(JSON.stringify(config), /legacy\/model|1234/);
	} finally {
		console.warn = originalWarn;
	}
	assert.equal(warnings.length, 1);
	assert.match(warnings[0], /ALEXANDRIA_AUTO_STORE/);
	assert.match(warnings[0], /ALEXANDRIA_EXTRACT_MODEL/);
	assert.match(warnings[0], /ALEXANDRIA_EXTRACT_TIMEOUT_MS/);
	assert.match(warnings[0], /store_memory.*explicitly/i);
});

test("ALEXANDRIA_REMINDERS=off disables reminders alone", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentJson,
		ALEXANDRIA_REMINDERS: "off",
	});
	assert.equal(config.remindersDisabled, true);
	assert.equal(config.recallDisabled, false);
});

test("client.json drives active fields but cannot supply credentials or enable legacy extraction", async () => {
	const warnings: string[] = [];
	const originalWarn = console.warn;
	console.warn = (message?: unknown) => warnings.push(String(message));
	let config: typeof import("../src/config.js").CONFIG;
	try {
		config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: fullJson });
	} finally {
		console.warn = originalWarn;
	}
	assert.equal(config.serverUrl, "http://127.0.0.1:9999/mcp");
	assert.equal(config.apiToken, undefined);
	assert.equal(config.projectId, undefined);
	assert.ok(!("storeDisabled" in config));
	assert.equal(config.recallDisabled, true);
	assert.equal(config.remindersDisabled, true);
	assert.equal(config.recallLimit, 7);
	assert.equal(config.recallMinSimilarity, 0.42);
	assert.equal(warnings.length, 1);
	assert.match(warnings[0], /store.enabled/);
	assert.match(warnings[0], /store.extract_model/);
	assert.match(warnings[0], /store.extract_timeout_ms/);
	// The target the server matches is trimmed on the way in, not sent padded.
	assert.equal(config.remindersProject, "from-json");
});

test("explicit env beats client.json in both directions", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullJson,
		ALEXANDRIA_URL: "http://127.0.0.1:4000/mcp",
		// Not "off": any set value counts as an explicit choice, so the file's
		// `enabled = false` cannot leave a feature dead after the user opts back in.
		ALEXANDRIA_AUTO_RECALL: "on",
		ALEXANDRIA_REMINDERS: "on",
		ALEXANDRIA_AUTO_RECALL_LIMIT: "3",
		ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY: "0.1",
	});
	assert.equal(config.serverUrl, "http://127.0.0.1:4000/mcp");
	assert.equal(config.recallDisabled, false);
	assert.equal(config.remindersDisabled, false);
	assert.equal(config.recallLimit, 3);
	assert.equal(config.recallMinSimilarity, 0.1);
});

test("API token and project ID are dedicated environment settings", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentJson,
		ALEXANDRIA_API_TOKEN: "companion-token",
		ALEXANDRIA_PROJECT_ID: " project:owned ",
		ANTHROPIC_API_KEY: "provider-token",
	});
	assert.equal(config.apiToken, "companion-token");
	assert.equal(config.projectId, "project:owned");
});

test("ALEXANDRIA_REMINDERS_PROJECT beats client.json and arrives trimmed", async () => {
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullJson,
		ALEXANDRIA_REMINDERS_PROJECT: "  alexandria  ",
	});
	assert.equal(config.remindersProject, "alexandria");
});

test("a whitespace-only ALEXANDRIA_REMINDERS_PROJECT falls through to the file", async () => {
	// A set-but-blank override must not be sent verbatim: the server matches the
	// target exactly, so a padded or empty project silently degrades delivery to
	// escalation-only. Falling through to the file keeps delivery working.
	const config = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullJson,
		ALEXANDRIA_REMINDERS_PROJECT: "   ",
	});
	assert.equal(config.remindersProject, "from-json");
});

test("an unparseable client.json falls back to defaults instead of throwing", async () => {
	// config.ts warns about the path it could not read — that line on stdout is
	// expected. What the prompt path depends on is a usable CONFIG either way.
	const config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: brokenJson });
	assert.equal(config.remindersDisabled, false);
	assert.equal(config.recallDisabled, false);
	assert.equal(config.recallLimit, 10);
	assert.equal(config.remindersProject, undefined);
});

test("an unreadable config warns before falling back to defaults", async (t) => {
	if (process.platform === "win32" || process.getuid?.() === 0) {
		t.skip("POSIX permissions are needed for this check");
		return;
	}
	const parent = join(workspace, "unreadable");
	await mkdir(parent);
	const path = join(parent, "client.json");
	await writeFile(path, '{"recall":{"enabled":false}}');
	const warnings: string[] = [];
	const originalWarn = console.warn;
	console.warn = (...args: unknown[]) => { warnings.push(args.map(String).join(" ")); };
	await chmod(parent, 0o000);
	try {
		const config = await configWith({ ALEXANDRIA_CLIENT_CONFIG: path });
		assert.equal(config.recallDisabled, false);
		assert.ok(warnings.some((warning) => warning.includes("failed to read or parse") && warning.includes(path)));
	} finally {
		await chmod(parent, 0o700);
		console.warn = originalWarn;
	}
});

test("a set-but-blank toggle does not re-enable what client.json disabled", async () => {
	// `ALEXANDRIA_REMINDERS=` in a direnv or .env is not a user saying "off", and
	// gating the file branch on `process.env.X === undefined` let a blank value win
	// the precedence race by being *present* — silently putting the consuming
	// per-prompt check_reminders call back on with no signal.
	for (const [key, field] of [
		["ALEXANDRIA_AUTO_RECALL", "recallDisabled"],
		["ALEXANDRIA_REMINDERS", "remindersDisabled"],
	] as const) {
		for (const blank of ["", "   "]) {
			const config = await configWith({
				ALEXANDRIA_CLIENT_CONFIG: fullJson,
				[key]: blank,
			});
			assert.equal(
				config[field],
				true,
				`${key}=${JSON.stringify(blank)} overrode the file's enabled=false`,
			);
		}
	}
});

test("an explicit env value still overrides the file in both directions", async () => {
	// The blank cases above must not have quietly broken the real contract.
	const off = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: absentJson,
		ALEXANDRIA_REMINDERS: "off",
	});
	assert.equal(off.remindersDisabled, true);
	const padded = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullJson,
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
	]) {
		for (const junk of ["", "  ", "ten", "1e999"]) {
			const config = await configWith({
				ALEXANDRIA_CLIENT_CONFIG: absentJson,
				[key]: junk,
			});
			const value =
				key === "ALEXANDRIA_AUTO_RECALL_LIMIT" ? config.recallLimit : config.recallMinSimilarity;
			assert.ok(Number.isFinite(value), `${key}=${JSON.stringify(junk)} -> ${value}`);
			assert.ok(value > 0, `${key}=${JSON.stringify(junk)} -> ${value}`);
		}
	}
	// A real number still wins over the file, and the file still wins over defaults.
	const numeric = await configWith({
		ALEXANDRIA_CLIENT_CONFIG: fullJson,
		ALEXANDRIA_AUTO_RECALL_LIMIT: "3",
	});
	assert.equal(numeric.recallLimit, 3);
	const fromFile = await configWith({ ALEXANDRIA_CLIENT_CONFIG: fullJson });
	assert.equal(fromFile.recallLimit, 7);
});
