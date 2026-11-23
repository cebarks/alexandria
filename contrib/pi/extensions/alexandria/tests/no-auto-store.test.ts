import { test } from "node:test";
import assert from "node:assert/strict";

process.env.ALEXANDRIA_CLIENT_CONFIG = "/tmp/alexandria-tests-absent-client.json";
process.env.ALEXANDRIA_API_TOKEN = "";
process.env.ALEXANDRIA_AUTO_RECALL = "on";
process.env.ALEXANDRIA_REMINDERS = "on";

const { default: extension } = await import("../src/index.js");

test("correction, preference, error and receipt prompts only run the prompt dispatcher", async () => {
	const handlers = new Map<string, Array<(...args: unknown[]) => unknown>>();
	const commands: string[] = [];
	const pi = {
		registerCommand(name: string) { commands.push(name); },
		on(event: string, handler: (...args: unknown[]) => unknown) {
			const list = handlers.get(event) ?? [];
			list.push(handler);
			handlers.set(event, list);
		},
	} as never;
	extension(pi);

	assert.deepEqual(commands, ["alexandria"], "the settings menu is the only new command");
	assert.deepEqual([...handlers.keys()].sort(), ["before_agent_start", "session_shutdown"]);
	const beforeAgentStart = handlers.get("before_agent_start")?.[0];
	assert.ok(beforeAgentStart, "the bounded recall/reminder prompt path remains registered");
	await beforeAgentStart(
		{ prompt: "No, use the corrected receipt. Always prefer the repaired flow. Error: previous attempt failed." },
		{
			cwd: process.cwd(),
			signal: undefined,
			ui: { notify() {} },
		},
	);

	// Tool-result/error/agent-end events used to pair observations and trigger stores.
	// They have no handler now, so replaying an error plus a successful receipt has no writer.
	assert.equal(handlers.has("tool_result"), false);
	assert.equal(handlers.has("tool_execution_end"), false);
	assert.equal(handlers.has("agent_end"), false);
	assert.equal(handlers.has("store_memory"), false);
	await handlers.get("session_shutdown")?.[0]({}, {});
});
