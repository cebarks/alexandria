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
process.env.ALEXANDRIA_AUTO_STORE = "off";
for (const key of ["ALEXANDRIA_AUTO_RECALL", "ALEXANDRIA_REMINDERS"]) delete process.env[key];
const { default: extension } = await import("../src/index.js");
type Handler = (event: any, ctx: any) => Promise<any>;
const handlers = new Map<string, Handler>();
const commands = new Map<string, { handler: (args: string, ctx: any) => Promise<void> }>();
let prewarms = 0;
let reminderCalls = 0;
const pi = {
	on(name: string, handler: Handler) { handlers.set(name, handler); },
	registerCommand(name: string, command: { handler: (args: string, ctx: any) => Promise<void> }) { commands.set(name, command); },
} as unknown as ExtensionAPI;
extension(pi, {
	prewarm: () => { prewarms++; },
	retrieveMemories: async () => { throw new Error("recall offline"); },
	checkReminders: async () => {
		reminderCalls++;
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
	assert.equal(reminderCalls, 1);

	await command.handler("", ctx);
	const failedRecall = await dispatch({ prompt: "second" }, ctx);
	assert.match(JSON.stringify(failedRecall), /take a break/);
	assert.equal(reminderCalls, 2);
	assert.ok(notifications.some((message) => /recall/i.test(message)));
});
