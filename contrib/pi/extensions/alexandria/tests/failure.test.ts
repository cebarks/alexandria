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

const { describeCause, classifyFailure, failureOf, asFailure, AlexandriaFailure } =
	await import("../src/failure.js");
type FailureContext = import("../src/failure.js").FailureContext;

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

// ── classifyFailure ────────────────────────────────────────────────────────────

const ctx = (over: Partial<FailureContext> = {}): FailureContext => ({
	aborted: false,
	...over,
});

test("an aborted signal is a cancellation, not an unreachable server", () => {
	// The SDK reports an aborted request as SdkErrorCode.REQUEST_TIMEOUT, so the
	// error alone cannot distinguish Esc from a dead server. Only the signal can.
	const c = classifyFailure(new Error("Request timed out"), ctx({ aborted: true }));
	assert.equal(c.kind, "cancelled");
	assert.equal(c.resetConnection, false, "a healthy session must survive an Esc");
});

test("cancellation outranks every other classification", () => {
	// A prompt the user abandoned while the worker was also unhealthy is still
	// primarily a cancellation: nothing should be torn down on the user's behalf.
	const c = classifyFailure(new Error("worker exited"),
		ctx({ aborted: true, workerFault: true, budgetExceeded: true }));
	assert.equal(c.kind, "cancelled");
	assert.equal(c.resetConnection, false);
});

test("the whole-prompt budget expiring is a stall, not a server timeout", () => {
	// With the per-call deadline owned by the worker, the main-thread budget is a
	// *delivery* deadline. If it fires without the worker reporting a server
	// timeout, the prompt path was slow — the server was not necessarily.
	const c = classifyFailure(new Error("Alexandria prompt budget (10000 ms) exceeded"),
		ctx({ budgetExceeded: true }));
	assert.equal(c.kind, "stalled");
	assert.equal(c.resetConnection, false);
});

test("a worker fault is attributed to the client and is recoverable", () => {
	const c = classifyFailure(new Error("worker exited"), ctx({ workerFault: true }));
	assert.equal(c.kind, "worker");
	assert.equal(c.resetConnection, true);
});

test("anything else is a transport failure and keeps the old reset behaviour", () => {
	const c = classifyFailure(new Error("Request timed out"), ctx());
	assert.equal(c.kind, "transport");
	assert.equal(c.resetConnection, true);
});

test("classification carries the rendered cause, not the raw error", () => {
	const inner = Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:3000"), {
		code: "ECONNREFUSED",
	});
	const outer = Object.assign(new TypeError("fetch failed"), { cause: inner });
	assert.match(classifyFailure(outer, ctx()).cause, /ECONNREFUSED/);
});

// ── AlexandriaFailure / failureOf ──────────────────────────────────────────────

test("AlexandriaFailure carries its classification through a rejection", () => {
	const f = new AlexandriaFailure("Request timed out", {
		kind: "transport",
		resetConnection: true,
	});
	assert.ok(f instanceof Error);
	assert.equal(f.name, "AlexandriaFailure");
	assert.equal(f.kind, "transport");
	assert.equal(f.message, "Request timed out");
});

test("failureOf reads a classification back off the rejection", () => {
	const f = new AlexandriaFailure("This operation was aborted", {
		kind: "cancelled",
		resetConnection: false,
	});
	const c = failureOf(f);
	assert.equal(c.kind, "cancelled");
	assert.equal(c.cause, "This operation was aborted");
	assert.equal(c.resetConnection, false);
});

test("failureOf falls back to transport for a throw site not yet converted", () => {
	// The pre-existing behaviour, so an unwrapped rejection cannot silently lose
	// its client reset while call sites are migrated one at a time.
	const c = failureOf(new Error("Request timed out"));
	assert.equal(c.kind, "transport");
	assert.equal(c.resetConnection, true);
});

test("failureOf preserves the cause chain of an unwrapped error", () => {
	const inner = Object.assign(new Error("getaddrinfo ENOTFOUND relativity.grv.st"), {
		code: "ENOTFOUND",
	});
	const c = failureOf(Object.assign(new TypeError("fetch failed"), { cause: inner }));
	assert.match(c.cause, /ENOTFOUND/);
});

// ── asFailure: the throw-site contract ─────────────────────────────────────

test("asFailure wraps an operator's Esc as a cancellation that keeps the session", () => {
	// The regression this pins end to end: the SDK reports the abort as
	// REQUEST_TIMEOUT, so without the signal context this reads as a dead server.
	const f = asFailure(new Error("Request timed out"), { aborted: true });
	assert.ok(f instanceof AlexandriaFailure);
	assert.equal(f.kind, "cancelled");
	assert.equal(f.resetConnection, false);
	// And buildInjection's reader agrees, so the verdict survives the rejection.
	assert.equal(failureOf(f).kind, "cancelled");
});

test("asFailure separates our own budget from a user cancellation", () => {
	// index.ts derives these from two different signals: ctx.signal.aborted means
	// the operator or pi cancelled; our budget firing with ctx.signal intact means
	// the prompt path was slow. Conflating them would hide a real stall behind
	// 'cancelled' and suppress a warning that should have been shown.
	const budget = asFailure(new Error("Alexandria prompt budget (10000 ms) exceeded"), {
		aborted: false,
		budgetExceeded: true,
	});
	assert.equal(budget.kind, "stalled");

	const userCancelled = asFailure(new Error("Request timed out"), {
		aborted: true,
		budgetExceeded: true,
	});
	assert.equal(userCancelled.kind, "cancelled", "a user cancel outranks the budget");
});

test("asFailure renders the deepest cause into the wrapped message", () => {
	const inner = Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:3000"), {
		code: "ECONNREFUSED",
	});
	const f = asFailure(Object.assign(new TypeError("fetch failed"), { cause: inner }), {
		aborted: false,
	});
	assert.equal(f.kind, "transport");
	assert.match(f.message, /ECONNREFUSED/);
	assert.doesNotMatch(f.message, /^fetch failed$/);
});
