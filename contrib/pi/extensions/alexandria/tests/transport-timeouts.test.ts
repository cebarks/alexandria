import { after, test } from "node:test";
import assert from "node:assert/strict";

const previousConfig = process.env.ALEXANDRIA_CLIENT_CONFIG;
const previousCallTimeout = process.env.ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS;
const previousBudget = process.env.ALEXANDRIA_PROMPT_BUDGET_MS;
const previousProjectId = process.env.ALEXANDRIA_PROJECT_ID;
process.env.ALEXANDRIA_CLIENT_CONFIG = "/tmp/alexandria-tests-absent-client.json";
process.env.ALEXANDRIA_PROJECT_ID = "project:timeout";
const recallSettings = { limit: 10, minSimilarity: 0.5 };
process.env.ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS = "30";
process.env.ALEXANDRIA_PROMPT_BUDGET_MS = "100";
const [
	{ retrieveMemories },
	{ checkReminders },
	{ PROMPT_CALL_TIMEOUT_MS },
	projectContext,
] = await Promise.all([
	import("../src/recall.js"),
	import("../src/reminders.js"),
	import("../src/mcp-client.js"),
	import("../src/project-context.js"),
]);

function abortAfter(ms: number): { signal: AbortSignal; cancel(): void } {
	const controller = new AbortController();
	const timer = setTimeout(() => controller.abort(new Error("prompt budget exceeded")), ms);
	return { signal: controller.signal, cancel: () => clearTimeout(timer) };
}

test(
	"prompt feature calls carry one shared cancellation signal and configured per-call bound",
	async () => {
		const { signal, cancel } = abortAfter(10_000);
		const observed: Array<{ name: string; timeout?: number; signal?: AbortSignal }> = [];
		const call = async (
			name: string,
			_args: Record<string, unknown>,
			timeout?: number,
			callSignal?: AbortSignal,
		) => {
			observed.push({ name, timeout, signal: callSignal });
			const text = name === "retrieve_memories" ? '{"results":[]}' : '{"delivered":[]}';
			return { content: [{ type: "text", text }] };
		};
		try {
			await retrieveMemories("test", call, signal, { ...recallSettings, projectId: "project:test" });
			await checkReminders("project:test", call, signal);
		} finally {
			cancel();
		}
		assert.deepEqual(observed.map((entry) => entry.name), ["retrieve_memories", "check_reminders"]);
		assert.ok(observed.every((entry) => entry.timeout === PROMPT_CALL_TIMEOUT_MS));
		assert.ok(observed.every((entry) => entry.signal === signal));
	},
);

test("a warm context fits the budget while a cache-miss resolution exhausts the same deadline", async () => {
	const authenticatedServer = {
		serverKey: "https://fixture.example/mcp",
		accountKey: "account-fixture",
		callTool: async (
			name: string,
			_args: Record<string, unknown>,
			_timeout?: number,
			signal?: AbortSignal,
		) => {
			await new Promise<void>((resolve, reject) => {
				const onAbort = () => {
					clearTimeout(timer);
					reject(signal?.reason ?? new Error("aborted"));
				};
				const timer = setTimeout(() => {
					signal?.removeEventListener("abort", onAbort);
					resolve();
				}, 20);
				signal?.addEventListener("abort", onAbort, { once: true });
			});
			const text =
				name === "resolve_project"
					? '{"project":{"id":"project:timeout"}}'
					: name === "retrieve_memories"
						? '{"results":[]}'
						: '{"delivered":[]}';
			return { content: [{ type: "text", text }] };
		},
	};
	const ctx = { cwd: "/synthetic/context" } as never;
	const { __resetProjectContext, getProjectContext } = projectContext;
	__resetProjectContext();
	const warmup = abortAfter(200);
	await getProjectContext(ctx, authenticatedServer, warmup.signal);
	warmup.cancel();
	const warmBudget = abortAfter(30);
	try {
		const project = await getProjectContext(ctx, authenticatedServer, warmBudget.signal);
		const outcomes = await Promise.all([
			retrieveMemories("q", authenticatedServer.callTool, warmBudget.signal, {
				...recallSettings,
				projectId: project.projectId,
			}),
			checkReminders(project.projectId, authenticatedServer.callTool, warmBudget.signal),
		]);
		assert.equal(outcomes.length, 2);
	} finally {
		warmBudget.cancel();
	}

	__resetProjectContext();
	const coldBudget = abortAfter(30);
	try {
		const project = await getProjectContext(ctx, authenticatedServer, coldBudget.signal);
		const coldOutcomes = await Promise.allSettled([
			retrieveMemories("q", authenticatedServer.callTool, coldBudget.signal, {
				...recallSettings,
				projectId: project.projectId,
			}),
			checkReminders(project.projectId, authenticatedServer.callTool, coldBudget.signal),
		]);
		assert.ok(coldOutcomes.every((outcome) => outcome.status === "rejected"));
	} finally {
		coldBudget.cancel();
		__resetProjectContext();
	}
});

test("a slow mocked transport aborts at the per-call timeout and keeps its project scope", async () => {
	const controller = new AbortController();
	const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
	const call = async (
		name: string,
		args: Record<string, unknown>,
		timeout?: number,
		signal?: AbortSignal,
	) => {
		calls.push({ name, args });
		assert.equal(timeout, 30);
		return await new Promise<never>((_resolve, reject) => {
			const onAbort = () => {
				clearTimeout(timer);
				reject(signal?.reason ?? new Error("aborted"));
			};
			const timer = setTimeout(() => {
				signal?.removeEventListener("abort", onAbort);
				reject(new Error("mock transport timeout"));
			}, timeout);
			signal?.addEventListener("abort", onAbort, { once: true });
		});
	};
	await assert.rejects(
		retrieveMemories("test", call, controller.signal, { ...recallSettings, projectId: "project:scoped" }),
		/mock transport timeout/,
	);
	assert.equal(calls.length, 1);
	assert.equal(calls[0].args.project, "project:scoped");
});

test("an aborted call rejects without retrying an unscoped request", async () => {
	const controller = new AbortController();
	const calls: Array<{ name: string; args: Record<string, unknown>; signal?: AbortSignal }> = [];
	const call = async (
		name: string,
		args: Record<string, unknown>,
		_timeout?: number,
		signal?: AbortSignal,
	) => {
		calls.push({ name, args, signal });
		controller.abort(new Error("cancelled"));
		throw signal?.reason;
	};
	await assert.rejects(
		retrieveMemories("test", call, controller.signal, { ...recallSettings, projectId: "project:scoped" }),
	);
	assert.equal(calls.length, 1);
	assert.deepEqual(calls[0].args, { query: "test", limit: 10, project: "project:scoped" });
});

after(() => {
	if (previousConfig === undefined) delete process.env.ALEXANDRIA_CLIENT_CONFIG;
	else process.env.ALEXANDRIA_CLIENT_CONFIG = previousConfig;
	if (previousCallTimeout === undefined) delete process.env.ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS;
	else process.env.ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS = previousCallTimeout;
	if (previousBudget === undefined) delete process.env.ALEXANDRIA_PROMPT_BUDGET_MS;
	else process.env.ALEXANDRIA_PROMPT_BUDGET_MS = previousBudget;
	if (previousProjectId === undefined) delete process.env.ALEXANDRIA_PROJECT_ID;
	else process.env.ALEXANDRIA_PROJECT_ID = previousProjectId;
});
