import { after, test } from "node:test";
import assert from "node:assert/strict";

const previousToken = process.env.ALEXANDRIA_API_TOKEN;
const previousConfig = process.env.ALEXANDRIA_CLIENT_CONFIG;
process.env.ALEXANDRIA_CLIENT_CONFIG = "/tmp/alexandria-tests-absent-client.toml";
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
