/**
 * Failure classification for the prompt path.
 *
 * Pure, and pure on purpose: like `injection.ts`, this exists because the
 * `before_agent_start` handler is an inline closure over imported functions and
 * cannot be reached from a test without module mocking, which hangs under tsx.
 * Nothing here reads CONFIG, touches the network, or imports the MCP SDK, so the
 * tests need no environment setup at all.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

const { describeCause } = await import("../src/failure.js");

test("names the OS cause undici hides under `fetch failed`", () => {
	// This is the defect the module exists for: undici wraps every connection
	// error in a `TypeError: fetch failed` and puts the real reason on `cause`,
	// so reading only `.message` made a closed port, a dead DNS name and an
	// unroutable host all render as the same useless string.
	const inner = Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:3000"), {
		code: "ECONNREFUSED",
	});
	const outer = Object.assign(new TypeError("fetch failed"), { cause: inner });
	const text = describeCause(outer);
	assert.match(text, /ECONNREFUSED/);
	assert.match(text, /127\.0\.0\.1:3000/);
	assert.doesNotMatch(text, /^fetch failed$/);
});

test("the deepest cause wins, not the shallowest", () => {
	const deepest = Object.assign(new Error("getaddrinfo ENOTFOUND relativity.grv.st"), {
		code: "ENOTFOUND",
	});
	const middle = Object.assign(new TypeError("fetch failed"), { cause: deepest });
	const outer = Object.assign(new Error("callTool failed"), { cause: middle });
	assert.match(describeCause(outer), /ENOTFOUND/);
});

test("a bare error with no cause renders its own message", () => {
	assert.equal(describeCause(new Error("Request timed out")), "Request timed out");
});

test("an error code is appended when the message omits it", () => {
	const e = Object.assign(new Error("Request timed out"), { code: "REQUEST_TIMEOUT" });
	assert.equal(describeCause(e), "Request timed out [REQUEST_TIMEOUT]");
});

test("a non-Error rejection is stringified, not thrown", () => {
	assert.equal(describeCause("socket hang up"), "socket hang up");
	assert.equal(describeCause(undefined), "unknown error");
	assert.equal(describeCause(null), "unknown error");
});

test("a cyclic cause chain terminates instead of hanging the prompt path", () => {
	// A cycle is legal JS, and hanging here would be worse than the bug being
	// diagnosed: this runs on the path that pi awaits before a turn starts.
	// The property under test is termination; the returned node is whatever the
	// deepest-wins rule reaches before the cycle detector fires.
	const a = new Error("a") as Error & { cause?: unknown };
	const b = new Error("b") as Error & { cause?: unknown };
	a.cause = b;
	b.cause = a;
	assert.equal(describeCause(a), "b");
	// And it must terminate from either end, not just the one that was walked first.
	assert.equal(describeCause(b), "a");
});

test("a chain deeper than the cap is truncated, not followed forever", () => {
	let cur: Error = Object.assign(new Error("root"), { code: "ROOT" });
	for (let i = 0; i < 50; i++) {
		cur = Object.assign(new Error(`level ${i}`), { cause: cur });
	}
	assert.match(describeCause(cur), /level/);
});
