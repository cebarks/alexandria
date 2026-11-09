import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { makeScratchDir } from "./helpers.js";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { RecallSettings } from "../src/recall-config.js";

const dir = await makeScratchDir("alexandria-menu-integration-");
const path = join(dir, "client.json");
writeFileSync(path, JSON.stringify({
	server: { url: "http://127.0.0.1:9/mcp" },
	recall: { enabled: false, limit: 4, min_similarity: 0.7 },
	reminders: { enabled: false },
}));
process.env.ALEXANDRIA_CLIENT_CONFIG = path;
process.env.XDG_STATE_HOME = dir;
process.env.ALEXANDRIA_AUTO_STORE = "off";
for (const key of ["ALEXANDRIA_AUTO_RECALL", "ALEXANDRIA_AUTO_RECALL_LIMIT", "ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY", "ALEXANDRIA_REMINDERS"]) {
	delete process.env[key];
}

const { default: extension } = await import("../src/index.js");
type Handler = (event: any, ctx: any) => Promise<any>;
type Command = { handler: (args: string, ctx: any) => Promise<void> };
const handlers = new Map<string, Handler>();
const commands = new Map<string, Command>();
let prewarms = 0;
const recalled: Array<{ query: string; settings: RecallSettings }> = [];
const reminders: string[] = [];
const pi = {
	on(name: string, handler: Handler) { handlers.set(name, handler); },
	registerCommand(name: string, command: Command) { commands.set(name, command); },
} as unknown as ExtensionAPI;

// External MCP calls are the only fakes; command registration, settings I/O,
// dispatch, and injection formatting are the real extension code.
extension(pi, {
	prewarm: () => { prewarms++; },
	retrieveMemories: async (query, _call, _signal, settings) => {
		recalled.push({ query, settings: settings as RecallSettings });
		return { items: [{ id: "fact:test", content: "remembered", similarity: 0.9 }] };
	},
	checkReminders: async () => { reminders.push("called"); return { items: [] }; },
});

function uiWith(selections: number[], inputs: string[] = []) {
	const notices: string[] = [];
	return {
		notices,
		ui: {
			select: async (_title: string, options: string[]) => {
				const index = selections.shift();
				return index === undefined ? undefined : options[index];
			},
			input: async () => inputs.shift(),
			notify: (message: string) => { notices.push(message); },
		},
	};
}

test("enable from a fully disabled startup persists and dispatches on the next prompt", async () => {
	const command = commands.get("alexandria");
	const dispatch = handlers.get("before_agent_start");
	assert.ok(command, "/alexandria must be registered");
	assert.ok(dispatch, "prompt dispatcher must exist when both features start disabled");
	const promptUi = uiWith([]);
	const ctx = { hasUI: true, cwd: dir, ui: promptUi.ui };
	assert.equal(await dispatch({ prompt: "before" }, ctx), undefined);
	assert.equal(prewarms, 0);
	assert.deepEqual(recalled, []);

	const menu = uiWith([0, 3]);
	await command.handler("", { hasUI: true, cwd: dir, ui: menu.ui });
	assert.equal(JSON.parse(readFileSync(path, "utf8")).recall.enabled, true);
	assert.equal(prewarms, 1);
	const result = await dispatch({ prompt: "after" }, ctx);
	assert.deepEqual(recalled, [{ query: "after", settings: { enabled: true, limit: 4, minSimilarity: 0.7 } }]);
	assert.match(JSON.stringify(result), /remembered/);
	assert.deepEqual(reminders, []);
});

test("editing limit and similarity changes subsequent prompt settings", async () => {
	const command = commands.get("alexandria");
	const dispatch = handlers.get("before_agent_start");
	assert.ok(command);
	assert.ok(dispatch);
	const menu = uiWith([1, 2, 3], ["8", "0.5"]);
	await command.handler("", { hasUI: true, cwd: dir, ui: menu.ui });
	const saved = JSON.parse(readFileSync(path, "utf8"));
	assert.equal(saved.recall.limit, 8);
	assert.equal(saved.recall.min_similarity, 0.5);
	await dispatch({ prompt: "new values" }, { hasUI: true, cwd: dir, ui: menu.ui });
	assert.deepEqual(recalled.at(-1), { query: "new values", settings: { enabled: true, limit: 8, minSimilarity: 0.5 } });

	const disable = uiWith([0, 3]);
	await command.handler("", { hasUI: true, cwd: dir, ui: disable.ui });
	assert.equal(JSON.parse(readFileSync(path, "utf8")).recall.enabled, false);
	const count = recalled.length;
	await dispatch({ prompt: "disabled again" }, { hasUI: true, cwd: dir, ui: disable.ui });
	assert.equal(recalled.length, count);
	assert.equal(prewarms, 1);
});

test("invalid input and cancellation leave the file unchanged", async () => {
	const command = commands.get("alexandria");
	assert.ok(command);
	const before = readFileSync(path, "utf8");
	const menu = uiWith([1, 3], ["0"]);
	await command.handler("", { hasUI: true, cwd: dir, ui: menu.ui });
	assert.equal(readFileSync(path, "utf8"), before);
	assert.ok(menu.notices.length > 0, "invalid limit must be reported");
	const cancelled = uiWith([]);
	await command.handler("", { hasUI: true, cwd: dir, ui: cancelled.ui });
	assert.equal(readFileSync(path, "utf8"), before);
	await command.handler("", { hasUI: false, cwd: dir, ui: cancelled.ui });
	assert.equal(readFileSync(path, "utf8"), before);
});
