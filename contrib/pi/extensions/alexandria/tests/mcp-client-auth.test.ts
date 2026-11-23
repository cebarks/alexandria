import { after, test } from "node:test";
import assert from "node:assert/strict";

const previousToken = process.env.ALEXANDRIA_API_TOKEN;
const previousConfig = process.env.ALEXANDRIA_CLIENT_CONFIG;
process.env.ALEXANDRIA_CLIENT_CONFIG = "/tmp/alexandria-tests-absent-client.json";
process.env.ALEXANDRIA_API_TOKEN = "companion-token";
const { authorizationHeaders, assertProjectResolutionSupported, authenticatedServer } = await import("../src/mcp-client.js");
after(() => {
	if (previousToken === undefined) delete process.env.ALEXANDRIA_API_TOKEN;
	else process.env.ALEXANDRIA_API_TOKEN = previousToken;
	if (previousConfig === undefined) delete process.env.ALEXANDRIA_CLIENT_CONFIG;
	else process.env.ALEXANDRIA_CLIENT_CONFIG = previousConfig;
});

test("companion authorization is independent of native MCP and provider credentials", () => {
	const previousToken = process.env.ALEXANDRIA_API_TOKEN;
	const previousProviderKey = process.env.ANTHROPIC_API_KEY;
	process.env.ALEXANDRIA_API_TOKEN = "companion-token";
	process.env.ANTHROPIC_API_KEY = "provider-token";
	try {
		assert.deepEqual(authorizationHeaders(), { Authorization: "Bearer companion-token" });
		assert.notEqual(authenticatedServer().accountKey, "companion-token");
	} finally {
		if (previousToken === undefined) delete process.env.ALEXANDRIA_API_TOKEN;
		else process.env.ALEXANDRIA_API_TOKEN = previousToken;
		if (previousProviderKey === undefined) delete process.env.ANTHROPIC_API_KEY;
		else process.env.ANTHROPIC_API_KEY = previousProviderKey;
	}
});

test("a resolved context cannot send a tool call under changed companion credentials", async (t) => {
	const snapshot = authenticatedServer();
	const previous = process.env.ALEXANDRIA_API_TOKEN;
	const requests = t.mock.method(globalThis, "fetch", async () => {
		throw new Error("test transport was reached");
	});
	try {
		process.env.ALEXANDRIA_API_TOKEN = "new-test-account-token";
		await assert.rejects(
			snapshot.callTool("retrieve_memories", { query: "fixture" }),
			/credentials or endpoint changed/i,
		);
		assert.equal(requests.mock.callCount(), 0);
	} finally {
		if (previous === undefined) delete process.env.ALEXANDRIA_API_TOKEN;
		else process.env.ALEXANDRIA_API_TOKEN = previous;
	}
});

test("a stale-session retry cannot reconnect under a changed companion endpoint", async (t) => {
	const previous = process.env.ALEXANDRIA_URL;
	process.env.ALEXANDRIA_URL = "http://127.0.0.1:3100/mcp";
	const snapshot = authenticatedServer();
	let attempts = 0;
	t.mock.method(globalThis, "fetch", async () => {
		attempts++;
		if (attempts === 1) {
			process.env.ALEXANDRIA_URL = "http://127.0.0.1:3101/mcp";
			throw new Error("test session not found");
		}
		throw new Error("mock transport was retried");
	});
	try {
		await assert.rejects(
			snapshot.callTool("retrieve_memories", { query: "fixture" }),
			/credentials or endpoint changed/i,
		);
		assert.equal(attempts, 1);
	} finally {
		if (previous === undefined) delete process.env.ALEXANDRIA_URL;
		else process.env.ALEXANDRIA_URL = previous;
	}
});

test("an unchanged companion identity can reconnect once after a stale session", async (t) => {
	const snapshot = authenticatedServer();
	let attempts = 0;
	t.mock.method(globalThis, "fetch", async () => {
		if (++attempts === 1) throw new Error("test session not found");
		throw new Error("mock reconnect reached");
	});
	await assert.rejects(snapshot.callTool("retrieve_memories", { query: "fixture" }), /mock reconnect reached/);
	assert.equal(attempts, 2);
});

test("missing companion token refuses to connect with actionable guidance", () => {
	assert.throws(() => authorizationHeaders(""), /ALEXANDRIA_API_TOKEN/);
});

test("missing project resolution capability returns explicit upgrade guidance", () => {
	assert.throws(
		() => assertProjectResolutionSupported(["retrieve_memories", "check_reminders"]),
		/upgrade.*resolve_project.*authentication/i,
	);
	assert.doesNotThrow(() => assertProjectResolutionSupported(["resolve_project"]));
});
