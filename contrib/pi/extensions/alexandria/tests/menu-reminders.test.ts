import { test } from "node:test";
import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { makeScratchDir } from "./helpers.js";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const dir = await makeScratchDir("alexandria-menu-reminders-");
const path = join(dir, "client.json");
writeFileSync(path, JSON.stringify({ recall: { enabled: false }, reminders: { enabled: true } }));
process.env.ALEXANDRIA_CLIENT_CONFIG = path;
process.env.XDG_STATE_HOME = dir;
process.env.ALEXANDRIA_PROJECT_ID = "project:menu-fixture";
for (const key of ["ALEXANDRIA_AUTO_RECALL", "ALEXANDRIA_REMINDERS"]) delete process.env[key];
const { default: extension } = await import("../src/index.js");
type Handler = (event: any, ctx: any) => Promise<any>;
const handlers = new Map<string, Handler>();
const commands = new Map<string, { handler: (args: string, ctx: any) => Promise<void> }>();
let prewarms = 0;
const reminderCalls: Array<{ project: string | undefined; signal?: AbortSignal }> = [];
const recallCalls: Array<{ project: string | undefined; signal?: AbortSignal }> = [];
const resolutions: Array<{ project: unknown; signal?: AbortSignal }> = [];
const pi = {
	on(name: string, handler: Handler) { handlers.set(name, handler); },
	registerCommand(name: string, command: { handler: (args: string, ctx: any) => Promise<void> }) { commands.set(name, command); },
} as unknown as ExtensionAPI;
const callTool: import("../src/mcp-client.js").CallTool = async (name, args, _timeout, signal) => {
	assert.equal(name, "resolve_project");
	resolutions.push({ project: args.project, signal });
	if (args.project !== "project:menu-fixture") return { isError: true };
	return { content: [{ type: "text", text: '{"project":{"id":"project:issued"}}' }] };
};
extension(pi, {
	authenticatedServer: () => ({ serverKey: "menu-fixture", accountKey: "menu-account", callTool }),
	prewarm: () => { prewarms++; },
	retrieveMemories: async (_query, call, signal, settings) => {
		assert.equal(call, callTool);
		recallCalls.push({ project: settings?.projectId, signal });
		throw new Error("recall offline");
	},
	checkReminders: async (project, call, signal) => {
		assert.equal(call, callTool);
		reminderCalls.push({ project, signal });
		return { items: [{ id: "reminder:1", message: "take a break" }] };
	},
});

const notifications: string[] = [];
const choices = [0, 3];
const ctx = {
	hasUI: true,
	cwd: dir,
	ui: {
		select: async (_title: string, options: string[]) => options[choices.shift() ?? 3],
		input: async () => undefined,
		notify: (message: string) => { notifications.push(message); },
	},
};

test("reminders work alone and survive a recall failure after recall is enabled", async () => {
	const dispatch = handlers.get("before_agent_start");
	const command = commands.get("alexandria");
	assert.ok(dispatch);
	assert.ok(command);
	assert.equal(prewarms, 1);
	const reminderOnly = await dispatch({ prompt: "first" }, ctx);
	assert.match(JSON.stringify(reminderOnly), /take a break/);
	assert.equal(reminderCalls.length, 1);
	assert.equal(reminderCalls[0].project, "project:issued");
	assert.equal(resolutions.length, 1);
	assert.ok(resolutions[0].signal instanceof AbortSignal);
	assert.equal(resolutions[0].signal, reminderCalls[0].signal);

	await command.handler("", ctx);
	const failedRecall = await dispatch({ prompt: "second" }, ctx);
	assert.match(JSON.stringify(failedRecall), /take a break/);
	assert.equal(reminderCalls.length, 2);
	assert.equal(recallCalls.length, 1);
	assert.equal(reminderCalls[1].project, "project:issued");
	assert.equal(recallCalls[0].project, reminderCalls[1].project);
	assert.equal(recallCalls[0].signal, reminderCalls[1].signal);
	assert.equal(resolutions.length, 1, "same authenticated project is cached");
	assert.ok(notifications.some((message) => /recall/i.test(message)));

	process.env.ALEXANDRIA_PROJECT_ID = "project:not-available";
	const refused = await dispatch({ prompt: "refused context" }, ctx);
	assert.equal(refused, undefined);
	assert.equal(resolutions.length, 2);
	assert.equal(reminderCalls.length, 2, "failed project resolution cannot consume Personal reminders");
	assert.equal(recallCalls.length, 1, "failed project resolution cannot read Personal memories");
});
