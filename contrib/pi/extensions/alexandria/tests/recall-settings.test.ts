import { test } from "node:test";
import assert from "node:assert/strict";
import { chmodSync, linkSync, lstatSync, mkdirSync, readFileSync, readlinkSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";
import { extensionRoot, makeScratchDir } from "./helpers.js";
import { showRecallSettings } from "../src/recall-settings.js";

process.env.ALEXANDRIA_CLIENT_CONFIG ??= "/tmp/alexandria-tests-absent-client.json";
const config = await import("../src/config.js");

interface RecallSettings {
	enabled: boolean;
	limit: number;
	minSimilarity: number;
}
type Key = keyof RecallSettings;
interface SettingsView {
	saved: RecallSettings;
	effective: RecallSettings;
	overrides: Partial<Record<Key, string>>;
}
interface SettingsStore {
	get(): SettingsView;
	save<K extends Key>(key: K, value: RecallSettings[K]): SettingsView;
}

function storeAt(path: string, env: NodeJS.ProcessEnv = {}): SettingsStore {
	const create = (config as unknown as {
		createRecallSettingsStore?: (path: string, env: NodeJS.ProcessEnv) => SettingsStore;
	}).createRecallSettingsStore;
	assert.ok(create, "recall settings must be writable from the menu");
	return create(path, env);
}

const dir = await makeScratchDir("alexandria-recall-settings-");

test("saving a recall setting preserves unrelated JSON and updates the live view", () => {
	const path = join(dir, "preserve.json");
	writeFileSync(path, JSON.stringify({
		server: { url: "http://example.invalid/mcp" },
		recall: { enabled: false, limit: 7, min_similarity: 0.42, future_key: "keep" },
		reminders: { enabled: false },
	}));
	const store = storeAt(path);
	const view = store.save("limit", 12);
	assert.deepEqual(view.effective, { enabled: false, limit: 12, minSimilarity: 0.42 });
	assert.deepEqual(store.get(), view);
	assert.deepEqual(JSON.parse(readFileSync(path, "utf8")), {
		server: { url: "http://example.invalid/mcp" },
		recall: { enabled: false, limit: 12, min_similarity: 0.42, future_key: "keep" },
		reminders: { enabled: false },
	});
});

test("the view distinguishes saved values from valid environment overrides", () => {
	const path = join(dir, "env.json");
	writeFileSync(path, JSON.stringify({ recall: { enabled: false, limit: 7, min_similarity: 0.42 } }));
	const store = storeAt(path, {
		ALEXANDRIA_AUTO_RECALL: " on ",
		ALEXANDRIA_AUTO_RECALL_LIMIT: "3",
		ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY: "not-a-number",
	});
	assert.deepEqual(store.get(), {
		saved: { enabled: false, limit: 7, minSimilarity: 0.42 },
		effective: { enabled: true, limit: 3, minSimilarity: 0.42 },
		overrides: { enabled: "ALEXANDRIA_AUTO_RECALL", limit: "ALEXANDRIA_AUTO_RECALL_LIMIT" },
	});
	const changed = store.save("limit", 9);
	assert.equal(changed.saved.limit, 9);
	assert.equal(changed.effective.limit, 3);
	assert.equal(changed.overrides.limit, "ALEXANDRIA_AUTO_RECALL_LIMIT");
});

test("the menu shows the effective override and warns when a saved value stays overridden", async () => {
	const path = join(dir, "menu-override.json");
	writeFileSync(path, '{"recall":{"limit":7}}');
	const store = storeAt(path, { ALEXANDRIA_AUTO_RECALL_LIMIT: "3" });
	const seen: string[][] = [];
	const notices: string[] = [];
	const choices = [1, 3];
	await showRecallSettings({
		select: async (_title, options) => { seen.push(options); return options[choices.shift() ?? 3]; },
		input: async () => "9",
		notify: (message) => { notices.push(message); },
	}, store, () => { throw new Error("must not prewarm"); });
	assert.match(seen[0][1], /3.*ALEXANDRIA_AUTO_RECALL_LIMIT.*saved 7/);
	assert.ok(notices.some((message) => /ALEXANDRIA_AUTO_RECALL_LIMIT.*effective/.test(message)));
	assert.equal(store.get().saved.limit, 9);
	assert.equal(store.get().effective.limit, 3);
	assert.equal(JSON.parse(readFileSync(path, "utf8")).recall.limit, 9);
});

test("an environment-disabled toggle is saved but never prewarmed as enabled", async () => {
	const path = join(dir, "toggle-override.json");
	writeFileSync(path, '{"recall":{"enabled":false}}');
	const store = storeAt(path, { ALEXANDRIA_AUTO_RECALL: "off" });
	const choices = [0, 3];
	const notices: string[] = [];
	let prewarms = 0;
	await showRecallSettings({
		select: async (_title, options) => options[choices.shift() ?? 3],
		input: async () => undefined,
		notify: (message) => { notices.push(message); },
	}, store, () => { prewarms++; });
	assert.equal(JSON.parse(readFileSync(path, "utf8")).recall.enabled, true);
	assert.equal(store.get().effective.enabled, false);
	assert.equal(prewarms, 0);
	assert.ok(notices.some((message) => message.includes("ALEXANDRIA_AUTO_RECALL")));
});

test("a prewarm failure does not report that a successful save failed", async () => {
	const path = join(dir, "prewarm.json");
	writeFileSync(path, '{"recall":{"enabled":false}}');
	const store = storeAt(path);
	const notices: string[] = [];
	const choices = [0, 3];
	await showRecallSettings({
		select: async (_title, options) => options[choices.shift() ?? 3],
		input: async () => undefined,
		notify: (message) => { notices.push(message); },
	}, store, () => { throw new Error("prewarm failed"); });
	assert.equal(JSON.parse(readFileSync(path, "utf8")).recall.enabled, true);
	assert.equal(store.get().effective.enabled, true);
	assert.ok(!notices.some((message) => /not saved/i.test(message)), notices.join("; "));
	assert.ok(notices.some((message) => /prewarm/i.test(message)));
});

test("a lock cleanup failure reports the committed save and updates live settings", () => {
	const path = join(dir, "cleanup-failure.json");
	writeFileSync(path, '{"recall":{"limit":4}}');
	const source = pathToFileURL(join(extensionRoot, "src/recall-config.ts")).href;
	const menu = pathToFileURL(join(extensionRoot, "src/recall-settings.ts")).href;
	const script = `
		import fs from "node:fs";
		import { syncBuiltinESMExports } from "node:module";
		const original = fs.unlinkSync;
		fs.unlinkSync = function(path, ...args) {
			if (path === process.env.CONFIG + ".lock") {
				const error = new Error("simulated cleanup failure"); error.code = "EACCES"; throw error;
			}
			return original.call(this, path, ...args);
		};
		syncBuiltinESMExports();
		const { createRecallSettingsStore } = await import(${JSON.stringify(source)});
		const { showRecallSettings } = await import(${JSON.stringify(menu)});
		const store = createRecallSettingsStore(process.env.CONFIG, {}), notices = [], choices = [1, 3];
		await showRecallSettings({
			select: async (_, options) => options[choices.shift()],
			input: async () => "5",
			notify: (message, type) => notices.push({message, type}),
		}, store, () => {});
		console.log(JSON.stringify({live: store.get().effective.limit,
			disk: JSON.parse(fs.readFileSync(process.env.CONFIG, "utf8")).recall.limit,
			lockRemains: fs.existsSync(process.env.CONFIG + ".lock"), notices}));`;
	const run = spawnSync(process.execPath, ["--import", "tsx", "--input-type=module", "-e", script], {
		cwd: extensionRoot, env: { ...process.env, CONFIG: path }, encoding: "utf8",
	});
	try {
		assert.equal(run.status, 0, run.stderr);
		const result = JSON.parse(run.stdout);
		assert.equal(result.disk, 5);
		assert.equal(result.live, 5);
		assert.equal(result.lockRemains, true);
		assert.ok(result.notices.some((notice: { message: string; type: string }) =>
			notice.type === "warning" && /saved.*lock/i.test(notice.message)));
		assert.ok(!result.notices.some((notice: { message: string }) => /not saved/i.test(notice.message)));
	} finally {
		if (lstatSync(`${path}.lock`, { throwIfNoEntry: false })) unlinkSync(`${path}.lock`);
	}
});

test("a missing config is created using the existing defaults", () => {
	const path = join(dir, "new", "client.json");
	const store = storeAt(path);
	assert.deepEqual(store.get().effective, { enabled: true, limit: 10, minSimilarity: 0.45 });
	store.save("enabled", false);
	assert.deepEqual(JSON.parse(readFileSync(path, "utf8")), { recall: { enabled: false } });
});

test("saving through a symlink preserves the link and target permissions", () => {
	const target = join(dir, "target.json");
	const path = join(dir, "linked.json");
	writeFileSync(target, '{"recall":{"enabled":true}}\n');
	chmodSync(target, 0o640);
	symlinkSync(target, path);
	const store = storeAt(path);
	store.save("enabled", false);
	assert.equal(lstatSync(path).isSymbolicLink(), true);
	assert.equal(readlinkSync(path), target);
	assert.equal(statSync(target).mode & 0o777, 0o640);
	assert.equal(JSON.parse(readFileSync(target, "utf8")).recall.enabled, false);
});

test("refuses to replace a hard-linked config and split its aliases", (t) => {
	if (process.platform === "win32") {
		t.skip("this check requires POSIX hard links");
		return;
	}
	const path = join(dir, "hardlinked-config.json");
	const alias = join(dir, "hardlinked-alias.json");
	const original = '{"recall":{"limit":4}}';
	writeFileSync(path, original);
	linkSync(path, alias);
	const store = storeAt(path);
	assert.throws(() => store.save("limit", 5), /hard.?link/i);
	assert.equal(readFileSync(path, "utf8"), original);
	assert.equal(readFileSync(alias, "utf8"), original);
	assert.equal(store.get().effective.limit, 4);
});

test("a symlink retargeted during a save does not update its old target", (t) => {
	if (process.platform === "win32") {
		t.skip("POSIX symlinks are needed for this check");
		return;
	}
	const parent = join(dir, "retargeted-link");
	mkdirSync(parent);
	const path = join(parent, "client.json");
	const first = join(parent, "first.json");
	const second = join(parent, "second.json");
	const original = '{"recall":{"limit":4}}';
	writeFileSync(first, original);
	writeFileSync(second, '{"recall":{"limit":7}}');
	symlinkSync(first, path);
	const source = pathToFileURL(join(extensionRoot, "src/recall-config.ts")).href;
	const script = `
		import fs from "node:fs";
		import { syncBuiltinESMExports } from "node:module";
		const open = fs.openSync, write = fs.writeFileSync;
		let tempFd, retargeted = false;
		fs.openSync = function(path, ...args) {
			const fd = open.call(this, path, ...args);
			if (typeof path === "string" && path.endsWith(".tmp")) tempFd = fd;
			return fd;
		};
		fs.writeFileSync = function(path, ...args) {
			const result = write.call(this, path, ...args);
			if (path === tempFd && !retargeted) {
				fs.unlinkSync(process.env.CONFIG); fs.symlinkSync(process.env.SECOND, process.env.CONFIG); retargeted = true;
			}
			return result;
		};
		syncBuiltinESMExports();
		const { createRecallSettingsStore } = await import(${JSON.stringify(source)});
		const store = createRecallSettingsStore(process.env.CONFIG, {});
		try { store.save("limit", 5); } catch {}
		console.log(JSON.stringify({retargeted, live: store.get().effective.limit}));`;
	const run = spawnSync(process.execPath, ["--import", "tsx", "--input-type=module", "-e", script], {
		cwd: extensionRoot, env: { ...process.env, CONFIG: path, SECOND: second }, encoding: "utf8",
	});
	assert.equal(run.status, 0, run.stderr);
	assert.deepEqual(JSON.parse(run.stdout), { retargeted: true, live: 4 });
	assert.equal(readFileSync(first, "utf8"), original);
	assert.equal(JSON.parse(readFileSync(second, "utf8")).recall.limit, 7);
	assert.equal(readlinkSync(path), second);
});

test("invalid JSON and non-object recall sections are never overwritten", () => {
	for (const [name, original] of [
		["invalid", "{ not json"],
		["root-array", "[]"],
		["bad-section", '{"recall": [1, 2]}'],
		["unsafe-integer", '{"other": 9007199254740993}'],
		["nonfinite-number", '{"other": 1e999}'],
	] as const) {
		const path = join(dir, `${name}.json`);
		writeFileSync(path, original);
		const store = storeAt(path);
		const before = store.get();
		assert.throws(() => store.save("enabled", false));
		assert.equal(readFileSync(path, "utf8"), original);
		assert.deepEqual(store.get(), before);
	}
});

test("refuses unrelated JSON numbers that would change during serialization", () => {
	for (const [name, number] of [
		["underflow", "1e-400"],
		["precise-decimal", "0.100000000000000005"],
		["negative-zero", "-0"],
	] as const) {
		const path = join(dir, `number-${name}.json`);
		const original = `{"other":${number},"recall":{"limit":4}}`;
		writeFileSync(path, original);
		const store = storeAt(path);
		assert.throws(() => store.save("limit", 5), /number|precision/i);
		assert.equal(readFileSync(path, "utf8"), original);
		assert.equal(store.get().effective.limit, 4);
	}
});

test("refuses duplicate JSON object keys before they can be discarded", () => {
	const path = join(dir, "duplicate-keys.json");
	const original = '{"other":{"name":1,"\\u006eame":2},"recall":{"limit":4}}';
	writeFileSync(path, original);
	const store = storeAt(path);
	assert.throws(() => store.save("limit", 5), /duplicate/i);
	assert.equal(readFileSync(path, "utf8"), original);
	assert.equal(store.get().effective.limit, 4);
});

test("round-trippable numeric forms and numeric text remain editable", () => {
	const path = join(dir, "number-safe.json");
	const original = '{"other":[1.2300e+3,1e-10,0.45],"text":"1e-400", "recall":{"limit":4}}';
	writeFileSync(path, original);
	const store = storeAt(path);
	store.save("limit", 5);
	assert.deepEqual(JSON.parse(readFileSync(path, "utf8")), {
		other: [1230, 1e-10, 0.45], text: "1e-400", recall: { limit: 5 },
	});
});

test("a competing writer's lock prevents changes and permits a later retry", () => {
	const path = join(dir, "locked.json");
	const original = '{"recall":{"enabled":true,"limit":10}}';
	writeFileSync(path, original);
	const store = storeAt(path);
	const before = store.get();
	writeFileSync(`${path}.lock`, "other writer\n");
	assert.throws(() => store.save("limit", 5), /locked.*stale lock/i);
	assert.equal(readFileSync(path, "utf8"), original);
	assert.deepEqual(store.get(), before);
	unlinkSync(`${path}.lock`);
	assert.equal(store.save("limit", 5).effective.limit, 5);
});

test("concurrent processes do not silently lose successful setting changes", { timeout: 10000 }, async () => {
	const path = join(dir, "concurrent.json");
	writeFileSync(path, '{"recall":{"limit":10,"min_similarity":0.45}}');
	const source = pathToFileURL(join(extensionRoot, "src/recall-config.ts")).href;
	const script = `
		import { createRecallSettingsStore } from ${JSON.stringify(source)};
		const store = createRecallSettingsStore(process.env.TEST_PATH, {});
		process.stdout.write("READY\\n");
		process.stdin.once("data", () => {
			try { store.save(process.env.TEST_KEY, Number(process.env.TEST_VALUE)); process.stdout.write("SAVED\\n"); }
			catch (err) { process.stdout.write("REJECTED " + err.message + "\\n"); }
		});`;
	function worker(key: string, value: number) {
		const child = spawn(process.execPath, ["--import", "tsx", "--input-type=module", "-e", script], {
			cwd: extensionRoot,
			env: { ...process.env, TEST_PATH: path, TEST_KEY: key, TEST_VALUE: String(value) },
		});
		let output = "";
		const ready = new Promise<void>((resolve, reject) => {
			child.stdout.on("data", (chunk: Buffer) => {
				output += chunk.toString();
				if (output.includes("READY\n")) resolve();
			});
			child.on("error", reject);
			child.on("exit", (code) => { if (!output.includes("READY\n")) reject(new Error(`worker exited ${code}`)); });
		});
		const done = new Promise<string>((resolve, reject) => {
			child.on("error", reject);
			child.on("close", (code) => code === 0 ? resolve(output) : reject(new Error(`worker exited ${code}`)));
		});
		return { child, ready, done };
	}
	const limit = worker("limit", 8);
	const similarity = worker("minSimilarity", 0.7);
	await Promise.all([limit.ready, similarity.ready]);
	limit.child.stdin.end("go\n");
	similarity.child.stdin.end("go\n");
	const outcomes = await Promise.all([limit.done, similarity.done]);
	for (const output of outcomes) assert.match(output, /SAVED|REJECTED .*locked/i);
	assert.ok(outcomes.some((output) => output.includes("SAVED")));
	const written = JSON.parse(readFileSync(path, "utf8")).recall;
	if (outcomes[0].includes("SAVED")) assert.equal(written.limit, 8);
	if (outcomes[1].includes("SAVED")) assert.equal(written.min_similarity, 0.7);
	const store = storeAt(path);
	if (!outcomes[0].includes("SAVED")) store.save("limit", 8);
	if (!outcomes[1].includes("SAVED")) store.save("minSimilarity", 0.7);
	assert.deepEqual(JSON.parse(readFileSync(path, "utf8")).recall, { limit: 8, min_similarity: 0.7 });
});

test("saves settings without external filesystem metadata utilities", (t) => {
	if (process.platform === "win32") {
		t.skip("this check targets POSIX file handling");
		return;
	}
	const path = join(dir, "no-metadata-tools.json");
	writeFileSync(path, '{"recall":{"limit":4}}');
	const source = pathToFileURL(join(extensionRoot, "src/recall-config.ts")).href;
	const script = `
		import { createRecallSettingsStore } from ${JSON.stringify(source)};
		const store = createRecallSettingsStore(process.env.CONFIG, {});
		store.save("limit", 5);
		console.log(JSON.stringify({live: store.get().effective.limit}));`;
	const run = spawnSync(process.execPath, ["--import", "tsx", "--input-type=module", "-e", script], {
		cwd: extensionRoot, env: { ...process.env, CONFIG: path, PATH: "/nonexistent" }, encoding: "utf8",
	});
	assert.equal(run.status, 0, run.stderr);
	assert.equal(JSON.parse(run.stdout).live, 5);
	assert.equal(JSON.parse(readFileSync(path, "utf8")).recall.limit, 5);
});

test("temporary-file substitution cannot chmod a victim or install a symlink", (t) => {
	if (process.platform === "win32") {
		t.skip("POSIX symlinks and modes are needed for this check");
		return;
	}
	const parent = join(dir, "temp-substitution");
	mkdirSync(parent);
	const path = join(parent, "client.json");
	const victim = join(parent, "victim");
	const original = '{"recall":{"limit":4}}';
	writeFileSync(path, original);
	chmodSync(path, 0o644);
	writeFileSync(victim, "test fixture");
	chmodSync(victim, 0o600);
	const source = pathToFileURL(join(extensionRoot, "src/recall-config.ts")).href;
	const script = `
		import fs from "node:fs";
		import { syncBuiltinESMExports } from "node:module";
		const open = fs.openSync, write = fs.writeFileSync;
		let tempFd, tempPath, swapped = false;
		fs.openSync = function(path, ...args) {
			const fd = open.call(this, path, ...args);
			if (typeof path === "string" && path.endsWith(".tmp")) { tempFd = fd; tempPath = path; }
			return fd;
		};
		fs.writeFileSync = function(path, ...args) {
			const result = write.call(this, path, ...args);
			if (path === tempFd && !swapped) {
				fs.unlinkSync(tempPath); fs.symlinkSync(process.env.VICTIM, tempPath); swapped = true;
			}
			return result;
		};
		syncBuiltinESMExports();
		const { createRecallSettingsStore } = await import(${JSON.stringify(source)});
		try { createRecallSettingsStore(process.env.CONFIG, {}).save("limit", 5); } catch {}
		console.log(JSON.stringify({ swapped, victimMode: fs.statSync(process.env.VICTIM).mode & 0o777,
			configSymlink: fs.lstatSync(process.env.CONFIG).isSymbolicLink(), config: fs.readFileSync(process.env.CONFIG, "utf8") }));`;
	const run = spawnSync(process.execPath, ["--import", "tsx", "--input-type=module", "-e", script], {
		cwd: extensionRoot, env: { ...process.env, CONFIG: path, VICTIM: victim }, encoding: "utf8",
	});
	assert.equal(run.status, 0, run.stderr);
	const result = JSON.parse(run.stdout);
	assert.equal(result.swapped, true);
	assert.equal(result.victimMode, 0o600);
	assert.equal(result.configSymlink, false);
	assert.equal(result.config, original);
});

test("a dangling symlink created during a new-file save is not replaced", (t) => {
	if (process.platform === "win32") {
		t.skip("POSIX symlinks are needed for this check");
		return;
	}
	const parent = join(dir, "new-file-symlink-race");
	mkdirSync(parent);
	const path = join(parent, "client.json");
	const missing = join(parent, "missing.json");
	const source = pathToFileURL(join(extensionRoot, "src/recall-config.ts")).href;
	const script = `
		import fs from "node:fs";
		import { syncBuiltinESMExports } from "node:module";
		const open = fs.openSync, write = fs.writeFileSync;
		let tempFd, inserted = false;
		fs.openSync = function(path, ...args) {
			const fd = open.call(this, path, ...args);
			if (typeof path === "string" && path.endsWith(".tmp")) tempFd = fd;
			return fd;
		};
		fs.writeFileSync = function(path, ...args) {
			const result = write.call(this, path, ...args);
			if (path === tempFd && !inserted) {
				fs.symlinkSync(process.env.MISSING, process.env.CONFIG); inserted = true;
			}
			return result;
		};
		syncBuiltinESMExports();
		const { createRecallSettingsStore } = await import(${JSON.stringify(source)});
		try { createRecallSettingsStore(process.env.CONFIG, {}).save("limit", 5); } catch {}
		console.log(JSON.stringify({ inserted, remains: fs.lstatSync(process.env.CONFIG).isSymbolicLink() }));`;
	const run = spawnSync(process.execPath, ["--import", "tsx", "--input-type=module", "-e", script], {
		cwd: extensionRoot, env: { ...process.env, CONFIG: path, MISSING: missing }, encoding: "utf8",
	});
	assert.equal(run.status, 0, run.stderr);
	assert.deepEqual(JSON.parse(run.stdout), { inserted: true, remains: true });
});

test("refuses a config in a directory writable by another user", (t) => {
	if (process.platform === "win32" || process.getuid?.() === 0) {
		t.skip("POSIX permissions are needed for this check");
		return;
	}
	const parent = join(dir, "shared-directory");
	mkdirSync(parent);
	chmodSync(parent, 0o777);
	const path = join(parent, "client.json");
	const original = '{"recall":{"limit":4}}';
	writeFileSync(path, original);
	const store = storeAt(path);
	try {
		assert.throws(() => store.save("limit", 5), /unsafe|untrusted|writable by/i);
		assert.equal(readFileSync(path, "utf8"), original);
		assert.equal(store.get().effective.limit, 4);
	} finally {
		chmodSync(parent, 0o700);
	}
});

test("an existing read-only config is not replaced even in a writable directory", (t) => {
	if (process.platform === "win32" || process.getuid?.() === 0) {
		t.skip("POSIX permissions are needed for this check");
		return;
	}
	const path = join(dir, "read-only-file.json");
	const original = '{"recall":{"limit":4}}';
	writeFileSync(path, original);
	chmodSync(path, 0o444);
	const store = storeAt(path);
	const before = store.get();
	try {
		assert.throws(() => store.save("limit", 5), /writ/i);
		assert.equal(readFileSync(path, "utf8"), original);
		assert.deepEqual(store.get(), before);
		assert.equal(statSync(path).mode & 0o777, 0o444);
	} finally {
		chmodSync(path, 0o600);
	}
});

test("an unwritable config location leaves live settings unchanged", (t) => {
	if (process.platform === "win32" || process.getuid?.() === 0) {
		t.skip("POSIX permissions are needed for this check");
		return;
	}
	const parent = join(dir, "readonly");
	mkdirSync(parent);
	const path = join(parent, "client.json");
	const store = storeAt(path);
	const before = store.get();
	chmodSync(parent, 0o500);
	try {
		assert.throws(() => store.save("enabled", false));
		assert.deepEqual(store.get(), before);
	} finally {
		chmodSync(parent, 0o700);
	}
});

test("invalid menu settings are rejected without modifying the file or live view", () => {
	const path = join(dir, "validation.json");
	const original = '{"recall":{"enabled":true,"limit":10,"min_similarity":0.45}}';
	writeFileSync(path, original);
	const store = storeAt(path);
	const before = store.get();
	for (const n of [0, 101, -1, 1.5, Number.NaN, Infinity]) {
		assert.throws(() => store.save("limit", n));
	}
	for (const n of [-1.01, 1.01, Number.NaN, Infinity]) {
		assert.throws(() => store.save("minSimilarity", n));
	}
	assert.equal(readFileSync(path, "utf8"), original);
	assert.deepEqual(store.get(), before);
});
