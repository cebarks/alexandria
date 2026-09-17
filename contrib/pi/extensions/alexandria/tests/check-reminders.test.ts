/**
 * The `check_reminders` payload contract, driven through the injected `call`
 * seam — every case here is a response shape the server (or a proxy, or an older
 * build) can produce, and the one thing they all have in common is that nothing
 * may throw out of `checkReminders`.
 *
 * Why that matters more here than anywhere else in the extension: `check_reminders`
 * *consumes* what it returns. A throw after the server has advanced a row destroys
 * that delivery permanently, and the dispatcher reads any throw as a transport
 * failure — so it also drops a healthy client and tells the user the network is at
 * fault.
 */

import { test } from "node:test";
import assert from "node:assert/strict";

process.env.ALEXANDRIA_CLIENT_CONFIG ??=
  "/tmp/alexandria-tests-absent-client.toml";

const { checkReminders, formatDueBlock, MAX_RENDERED_REMINDERS } =
  await import("../src/reminders.js");
type CallTool = import("../src/mcp-client.js").CallTool;

/** A stub transport that answers with whatever the MCP layer would hand back. */
function callReturning(payload: unknown, isError = false): CallTool {
  return async () => ({
    isError,
    content: [{ type: "text", text: JSON.stringify(payload) }],
  });
}

const delivered = (...rows: unknown[]) => ({ status: "ok", delivered: rows });

test("a normal delivery reaches the renderer intact", async () => {
  const { items, error } = await checkReminders(
    "alexandria",
    callReturning(
      delivered({
        id: "reminder:1",
        message: "ship it",
        due_at: "2026-09-16T07:30:00Z",
      }),
    ),
  );
  assert.equal(error, undefined);
  assert.equal(items.length, 1);
  assert.equal(items[0].message, "ship it");
  assert.equal(items[0].due_at, "2026-09-16T07:30:00Z");
});

test("the project hint is passed through, and omitted when there is none", async () => {
  const seen: Record<string, unknown>[] = [];
  const spy: CallTool = async (_name, args) => {
    seen.push(args);
    return { content: [{ type: "text", text: '{"delivered":[]}' }] };
  };
  await checkReminders("alexandria", spy);
  await checkReminders(undefined, spy);
  assert.deepEqual(seen, [{ project: "alexandria" }, {}]);
});

test("an error-shaped response is reported, not rendered as 'nothing due'", async () => {
  for (const [payload, isError] of [
    [{ status: "error", message: "no such table" }, true],
    [{ status: "error", message: "no such table" }, false],
    [{ status: "error" }, false],
  ] as const) {
    const { items, error } = await checkReminders(
      undefined,
      callReturning(payload, isError),
    );
    assert.deepEqual(items, [], JSON.stringify(payload));
    assert.ok(error, `${JSON.stringify(payload)} must surface an error`);
  }
});

test("a response with no text block is a failure, not an empty delivery", async () => {
  // README promises "a warning notification at most" for a malformed response;
  // reporting nothing-due here would hide a delivery path that never works.
  const { items, error } = await checkReminders(undefined, async () => ({
    content: [{ type: "image", data: "…" }],
  }));
  assert.deepEqual(items, []);
  assert.match(error ?? "", /no text content/);

  const unparsed = await checkReminders(undefined, async () => ({
    content: [{ type: "text", text: "<html>502 bad gateway</html>" }],
  }));
  assert.match(unparsed.error ?? "", /malformed check_reminders response/);
});

test("a wrong-typed field costs its marker, never the reminder", async () => {
  // The regression: `formatDueBlock` renders each field through a string
  // operation, and one non-string value used to throw a TypeError out of the
  // task — which the dispatcher classified as a transport failure, resetting a
  // healthy client *after* the rows were consumed.
  const { items, error } = await checkReminders(
    undefined,
    callReturning(
      delivered({
        id: "reminder:9",
        message: "rotate the signing key",
        due_at: 1_758_000_000, // a number, not an RFC 3339 string
        target: { nope: true }, // an object, not a string
        note: null,
        missed_occurrences: "lots",
        escalated: "yes",
        recurring: 1,
        provenance: "origin",
      }),
    ),
  );
  assert.equal(error, undefined);
  assert.equal(items.length, 1);
  const bullet = formatDueBlock(items).split("\n")[1];
  assert.match(bullet, /rotate the signing key/);
  assert.match(bullet, /\[id reminder:9\]/);
  // The unusable fields are simply absent rather than rendered as `[object Object]`.
  assert.doesNotMatch(bullet, /\[target/);
  assert.doesNotMatch(bullet, /missed/);
  assert.doesNotMatch(bullet, /undefined/);
});

test("rows that carry nothing renderable are dropped, and the rest survive", async () => {
  const { items } = await checkReminders(
    undefined,
    callReturning(
      delivered(
        null,
        "not even an object",
        42,
        {}, // no id, no message → nothing to say
        { id: "reminder:1", message: "" }, // no text but an addressable id
      ),
    ),
  );
  assert.equal(items.length, 1);
  assert.equal(items[0].id, "reminder:1");
  // A message-less row still has to be counted and rendered as a placeholder —
  // it was consumed server-side.
  assert.match(formatDueBlock(items), /\(reminder with no text\)/);
});

test("a saturated missed count is distinguishable from an exact one", async () => {
  const { items } = await checkReminders(
    undefined,
    callReturning(
      delivered({
        id: "reminder:1",
        message: "metronome",
        missed_occurrences: 10_000,
        missed_occurrences_saturated: true,
      }),
    ),
  );
  assert.equal(items[0].missed_occurrences_saturated, true);
  assert.match(
    formatDueBlock(items),
    /missed 10000\+ earlier occurrence\(s\), count capped/,
  );
});

test("a pile-up degrades to a pointer, not a wall of bullets", async () => {
  const rows = Array.from({ length: MAX_RENDERED_REMINDERS + 5 }, (_, i) => ({
    id: `reminder:${i}`,
    message: `nag ${i}`,
  }));
  const { items } = await checkReminders(
    undefined,
    callReturning(delivered(...rows)),
  );
  assert.equal(items.length, rows.length, "every row was consumed server-side");
  const rendered = formatDueBlock(items).split("\n");
  assert.equal(rendered.length, 1 + MAX_RENDERED_REMINDERS + 1);
  assert.match(
    rendered[rendered.length - 1],
    /…and 5 more consumed in this check — call list_reminders/,
  );
});

test("local times and the zone are what reach the agent", async () => {
  const { items } = await checkReminders(
    undefined,
    callReturning(
      delivered({
        id: "reminder:1",
        message: "standup",
        due_at: "2026-09-16T07:30:00Z",
        due_at_local: "2026-09-16 09:30 CEST",
        timezone: "Europe/Berlin",
        recurring: true,
        next_due_at: "2026-09-17T07:30:00Z",
        next_due_at_local: "2026-09-17 09:30 CEST",
      }),
    ),
  );
  const bullet = formatDueBlock(items).split("\n")[1];
  assert.match(bullet, /\(due 2026-09-16 09:30 CEST\)/);
  assert.doesNotMatch(
    bullet,
    /Europe\/Berlin/,
    "the IANA name is payload data, not bullet noise",
  );
  assert.equal(items[0].timezone, "Europe/Berlin");
  assert.match(bullet, /next fire 2026-09-17 09:30 CEST/);
  assert.doesNotMatch(
    bullet,
    /07:30:00Z/,
    "the UTC spelling is the fallback only",
  );
});
