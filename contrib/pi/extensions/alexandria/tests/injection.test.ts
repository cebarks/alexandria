/**
 * The merged dispatcher's isolation contract, tested through `buildInjection` —
 * the pure seam the `before_agent_start` handler now delegates to.
 *
 * This is the coverage the design plan asked for ("Per-feature failure isolation
 * (one feature throws → others still deliver)") and which the handler itself could
 * not be given: as an inline closure over imported functions it is unreachable
 * from a test without module mocking, which hangs under tsx. Extracting the merge
 * step is what makes the claim checkable, and every row below is a case where the
 * old inline code was asserted only by reading it.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

process.env.ALEXANDRIA_CLIENT_CONFIG ??= "/tmp/alexandria-tests-absent-client.toml";

const { buildInjection } = await import("../src/injection.js");
type FeatureOutcome = import("../src/injection.js").FeatureOutcome;

const ok = <T>(value: T) => ({ status: "fulfilled" as const, value });
const bad = (reason: unknown) => ({ status: "rejected" as const, reason });

const RECALL: FeatureOutcome = { block: "RECALL-BLOCK" };
const REMINDERS: FeatureOutcome = { block: "REMINDER-BLOCK", count: 2 };

test("both features succeed: one message, recall first, reminders second", () => {
	const out = buildInjection(ok(RECALL), ok(REMINDERS));
	assert.equal(out.message?.customType, "alexandria");
	assert.equal(out.message?.content, "RECALL-BLOCK\n\nREMINDER-BLOCK");
	assert.equal(out.resetClient, false);
	// The human-visible half of a delivery is announced once, and says what the
	// agent was given rather than promising the user saw it.
	assert.equal(out.notifications.length, 1);
	assert.match(out.notifications[0].text, /2 Alexandria reminder\(s\) due/);
	assert.equal(out.notifications[0].level, "info");
});

test("one feature rejecting never suppresses the other's injection", () => {
	const recallOnly = buildInjection(bad(new Error("socket down")), ok(REMINDERS));
	assert.equal(recallOnly.message?.content, "REMINDER-BLOCK");
	// The reminder delivery must still be announced: an isolated recall failure
	// cannot be allowed to swallow the human half of a consumed reminder.
	assert.ok(
		recallOnly.notifications.some((n) => /reminder\(s\) due/.test(n.text)),
		JSON.stringify(recallOnly.notifications),
	);

	const remindersOnly = buildInjection(ok(RECALL), bad(new Error("timeout")));
	assert.equal(remindersOnly.message?.content, "RECALL-BLOCK");
});

test("both rejecting returns no message at all rather than a partial one", () => {
	const out = buildInjection(
		bad(new Error("ECONNREFUSED")),
		bad(new Error("ECONNREFUSED")),
	);
	assert.equal(out.message, undefined);
	// One shared cause is said once — two identical warnings read as a bug in the
	// reporter rather than one outage.
	const warnings = out.notifications.filter((n) => n.level === "warning");
	assert.equal(warnings.length, 1);
	assert.match(warnings[0].text, /without recall or reminders/);
	assert.equal(out.resetClient, true);
});

test("distinct causes are both reported, or the diagnostic names the wrong subsystem", () => {
	const out = buildInjection(
		bad(new Error("recall exploded")),
		bad(new Error("reminder check timed out")),
	);
	assert.equal(out.notifications.length, 1);
	assert.match(out.notifications[0].text, /recall exploded/);
	assert.match(out.notifications[0].text, /reminder check timed out/);
});

test("a rejection drops the shared client; a server-reported error does not", () => {
	// The distinction is load-bearing: a cached connect rejection used to be
	// replayed forever, so delivery never resumed — but resetting on a payload the
	// server answered kills a healthy connection for nothing.
	assert.equal(buildInjection(bad(new Error("down")), ok({ block: null })).resetClient, true);
	assert.equal(
		buildInjection(ok({ block: null, error: "malformed response" }), ok({ block: null })).resetClient,
		false,
	);
});

test("an empty delivery warns without injecting an empty block", () => {
	const out = buildInjection(ok({ block: null }), ok({ block: null, error: "status error" }));
	assert.equal(out.message, undefined);
	const warnings = out.notifications.filter((n) => n.level === "warning");
	assert.equal(warnings.length, 1);
	assert.match(warnings[0].text, /reminders check failed \(status error\)/);
	assert.match(warnings[0].text, /nothing was consumed/);
});

test("a recall payload problem is named the same way, and says it is not fatal", () => {
	const out = buildInjection(ok({ block: null, error: "no results list" }), ok({ block: null }));
	assert.equal(out.message, undefined);
	assert.equal(out.notifications.length, 1);
	assert.match(out.notifications[0].text, /auto-recall failed \(no results list\)/);
	assert.match(out.notifications[0].text, /continuing without it/);
});

test("both disabled: nothing injected, nothing said, no client churn", () => {
	// The disabled branches resolve to a neutral value rather than being skipped,
	// so no early return can suppress the other feature.
	const out = buildInjection(ok({ block: null }), ok({ block: null, count: 0 }));
	assert.equal(out.message, undefined);
	assert.deepEqual(out.notifications, []);
	assert.equal(out.resetClient, false);
});

test("a rejection reason that is not an Error still produces readable text", () => {
	const out = buildInjection(bad("string failure"), ok({ block: null }));
	assert.match(out.notifications[0].text, /string failure/);
});
