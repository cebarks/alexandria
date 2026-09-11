import test from "node:test";
import assert from "node:assert/strict";
import { finalizeError, storeError } from "./finalize-result.ts";

test("ok result is not an error", () => {
	assert.equal(finalizeError('{"status":"ok","external_id":"abc"}'), undefined);
});

test("session not found is the expected chat-only case", () => {
	assert.equal(
		finalizeError('{"status":"error","message":"Session not found: abc-123"}'),
		undefined,
	);
});

test("any other server error is surfaced", () => {
	assert.equal(
		finalizeError('{"status":"error","message":"summary too long"}'),
		"summary too long",
	);
});

test("unparseable or missing body is surfaced", () => {
	assert.equal(finalizeError(undefined), "empty finalize_session result");
	assert.equal(finalizeError("not json"), "unparseable finalize_session result: not json");
});

test("store ok result is not an error", () => {
	assert.equal(storeError('{"status":"ok","id":"fact:abc"}'), undefined);
});

test("store server error is surfaced", () => {
	assert.equal(
		storeError('{"status":"error","message":"content too long"}'),
		"content too long",
	);
});

test("store unparseable or missing body is surfaced", () => {
	assert.equal(storeError(undefined), "empty store_memory result");
	assert.equal(storeError("not json"), "unparseable store_memory result: not json");
});
