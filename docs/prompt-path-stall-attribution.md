# Prompt-Path Stall Attribution & Worker-Isolated MCP Client

> **STATUS (2026-09-22): partially superseded.** Tasks 1–3 were implemented as
> written. Tasks 4–7 (the worker) were **not**, and the `worker` failure kind they
> introduced was removed rather than left without a producer.
>
> Better-controlled measurement narrowed the fault to the **cold handshake only**:
> freezing pi's loop 6 s ten milliseconds into a *first* call produced a false
> `REQUEST_TIMEOUT` in 6/6 fresh processes, the same freeze against an
> already-connected client resolved normally in 15/15, and warm-call races came back
> clean. A worker thread would have fixed a window that turned out not to exist, at
> the cost of a plain-`.mjs` entry point (pi loads extensions through a bundled jiti
> a spawned worker cannot inherit).
>
> What shipped instead: `prewarm()` connects at extension load so no handshake is in
> flight when a prompt starts (3/3 honoured), and `src/diag.ts` logs one JSON line
> per prompt — including `cold` and `driftMs` — so the residual reconnect window is
> observable if it ever fires. See the branch
> `fix/prompt-path-prewarm-and-diagnostics`.
>
> The original plan is retained below unchanged: it is the reasoning that led to the
> measurements, and Task 8's verification method is what disproved it.

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Stop blaming the Alexandria server for client-side event-loop stalls, make genuine connection failures name their real cause, and move the MCP client onto a worker thread so a blocked pi main loop can no longer discard an already-received response.

**Architecture:** Three separable layers. (1) A pure `failure.ts` that classifies a rejection into `cancelled | stalled | worker | transport` and renders the deepest `err.cause` — the seam `buildInjection` consumes instead of `reasonText`. (2) A `worker-bridge.ts` state machine that speaks a request/response protocol over an *injected* transport, so correlation, cancellation forwarding, watchdog, crash-recovery and retry-once are all testable with no thread and no socket. (3) A thin `mcp-client.ts` that wires a real `node:worker_threads` Worker into the bridge and owns the `Client` + all SDK timers off the main loop. `callToolWithRetry`'s exported signature is preserved exactly, so `recall.ts`, `reminders.ts` and their existing injected-`CallTool` tests are untouched.

**Tech Stack:** TypeScript (ESM, `.js` import specifiers), `node:worker_threads`, `node:test` + `tsx`, `@modelcontextprotocol/client` 2.0.0.

---

## Why (measured, not assumed)

The bug: `Alexandria unreachable (Request timed out)` fires on relativity over **loopback**, against a server measured at p50 45ms / max 116ms over 40 sequential calls, 64-way concurrency max 395ms with zero failures, wet-vs-dry write path identical (p50 83 vs 82ms) under ~55% aggregate iowait, `NRestarts=0`, HNSW index armed.

Reproduced cause: the SDK arms its per-request deadline as a `setTimeout` **on the event loop the extension shares with every other in-process pi extension**. Block that loop past the remaining budget after the request is sent and the overdue timer wins the race against a response already sitting in the socket buffer:

```
server=http://127.0.0.1:3000/mcp budget=5000ms inflightDelay=10ms block=6000ms
attempt 1: sent@30ms blocked->6030ms  THREW code=REQUEST_TIMEOUT "Request timed out"
```

Trigger class is real: pi-lens (15) and pi-subagents (30) are in-process extensions with non-test `execSync`/`spawnSync` call sites; pi core does synchronous session-file read/write.

Two further confirmed defects fixed here:
- `reasonText()` returns only `err.message`, so undici's `fetch failed` hides `err.cause`: a closed port and an NXDOMAIN both render identically.
- A **pre-aborted signal is classified `REQUEST_TIMEOUT` by the SDK**, so hitting Esc mid-prompt renders a warning *and* sets `resetClient=true`, dropping a healthy session.

Note the control experiment that must not be repeated: blocking the loop **before** the send proves nothing, because the timer only arms once the request is written. That variant "passed" and is a false negative.

**Consequence for scope:** once the SDK timer lives in the worker, the late response is simply *used* when the main loop resumes — recall is delayed, not lost. So there is no stall-classified call failure left to retry. "Retry once" is therefore scoped to **worker-side fault** (crash/wedge → respawn and retry), never to a slow server, where retrying would only double the wait.

---

### Task 1: `describeCause` — walk the cause chain

**Files:**
- Create: `contrib/pi/extensions/alexandria/src/failure.ts`
- Test: `contrib/pi/extensions/alexandria/tests/failure.test.ts`

**Step 1: Write the failing test**

```ts
import { test } from "node:test";
import assert from "node:assert/strict";

process.env.ALEXANDRIA_CLIENT_CONFIG ??= "/tmp/alexandria-tests-absent-client.toml";

const { describeCause } = await import("../src/failure.js");

test("names the OS cause undici hides under `fetch failed`", () => {
	const inner = Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:3000"), {
		code: "ECONNREFUSED",
	});
	const outer = Object.assign(new TypeError("fetch failed"), { cause: inner });
	const text = describeCause(outer);
	assert.match(text, /ECONNREFUSED/);
	assert.match(text, /127\.0\.0\.1:3000/);
	// The wrapper alone is the useless half; it must not be what the user sees.
	assert.doesNotMatch(text, /^fetch failed$/);
});

test("a bare error with no cause renders its own message", () => {
	assert.equal(describeCause(new Error("Request timed out")), "Request timed out");
});

test("a non-Error rejection is stringified, not thrown", () => {
	assert.equal(describeCause("socket hang up"), "socket hang up");
	assert.equal(describeCause(undefined), "undefined");
});

test("a cyclic cause chain terminates instead of hanging", () => {
	const a = new Error("a") as Error & { cause?: unknown };
	const b = new Error("b") as Error & { cause?: unknown };
	a.cause = b;
	b.cause = a;
	assert.match(describeCause(a), /a/);
});
```

**Step 2: Run it, verify it fails**

Run: `cd contrib/pi/extensions/alexandria && npx tsx --test tests/failure.test.ts`
Expected: FAIL — cannot find module `../src/failure.js`.

**Step 3: Implement**

`src/failure.ts`:

```ts
/**
 * Failure classification for the prompt path.
 *
 * Extracted as a pure module for the reason `injection.ts` was: the
 * `before_agent_start` handler is an inline closure over imported functions and
 * cannot be reached from a test without module mocking, which hangs under tsx.
 */

/** Cap on cause-chain depth. undici nests one or two levels; anything deeper is
 *  a cycle or a library bug, and neither is worth more text in a toast. */
const MAX_CAUSE_DEPTH = 6;

/**
 * The user-facing form of a rejection: the *deepest* cause's message, with its
 * `code` when it has one.
 *
 * `err.message` alone is actively misleading for transport failures, because
 * undici wraps every connection error in a `TypeError: fetch failed` and puts the
 * real reason on `cause`. Reading only the wrapper makes a closed port, a dead
 * DNS name and an unroutable host all render as the same useless string.
 *
 * A `seen` set rather than a depth counter alone: a cyclic `cause` is legal JS and
 * would otherwise hang the prompt path — the exact thing this module exists to
 * keep off it.
 */
export function describeCause(err: unknown): string {
	const parts: string[] = [];
	const seen = new Set<unknown>();
	let cur: unknown = err;
	for (let i = 0; i < MAX_CAUSE_DEPTH && cur !== undefined && cur !== null; i++) {
		if (seen.has(cur)) break;
		seen.add(cur);
		if (!(cur instanceof Error)) {
			parts.push(String(cur));
			break;
		}
		const coded = cur as Error & { code?: unknown };
		parts.push(
			typeof coded.code === "string" && coded.code !== ""
				? `${cur.message} [${coded.code}]`
				: cur.message,
		);
		cur = (cur as Error & { cause?: unknown }).cause;
	}
	// Deepest last, and the deepest is the informative one.
	return parts.length === 0 ? "unknown error" : parts[parts.length - 1];
}
```

**Step 4: Run it, verify it passes**

Run: `npx tsx --test tests/failure.test.ts`
Expected: PASS (4 tests).

**Step 5: Commit**

`git add -A && git commit -m "fix(pi): name the real cause of a transport failure instead of 'fetch failed'"`

---

### Task 2: `classifyFailure` + `AlexandriaFailure`

**Files:**
- Modify: `contrib/pi/extensions/alexandria/src/failure.ts`
- Test: `contrib/pi/extensions/alexandria/tests/failure.test.ts` (append)

**Step 1: Write the failing test**

```ts
const { classifyFailure, AlexandriaFailure } = await import("../src/failure.js");

test("an aborted signal is a cancellation, not an unreachable server", () => {
	// The SDK reports an aborted request as SdkErrorCode.REQUEST_TIMEOUT, so the
	// error alone cannot distinguish Esc from a dead server. Only the signal can.
	const c = classifyFailure(new Error("Request timed out"), { aborted: true });
	assert.equal(c.kind, "cancelled");
	assert.equal(c.resetConnection, false, "a healthy session must survive an Esc");
});

test("a worker fault is attributed to the client, and is recoverable", () => {
	const c = classifyFailure(new Error("worker exited"), {
		aborted: false,
		workerFault: true,
	});
	assert.equal(c.kind, "worker");
	assert.equal(c.resetConnection, true);
});

test("anything else is a transport failure and keeps the old reset behaviour", () => {
	const c = classifyFailure(new Error("Request timed out"), { aborted: false });
	assert.equal(c.kind, "transport");
	assert.equal(c.resetConnection, true);
});

test("the 10s budget expiring is a stall, not a server timeout", () => {
	// With the SDK deadline in the worker, the main-thread budget is a *delivery*
	// deadline. If it fires without the worker reporting a server timeout, the
	// server was not the thing that was slow.
	const c = classifyFailure(new Error("Alexandria prompt budget (10000 ms) exceeded"), {
		aborted: false,
		budgetExceeded: true,
	});
	assert.equal(c.kind, "stalled");
	assert.equal(c.resetConnection, false);
});

test("AlexandriaFailure carries its classification through a rejection", () => {
	const f = new AlexandriaFailure("Request timed out", {
		kind: "transport",
		resetConnection: true,
	});
	assert.ok(f instanceof Error);
	assert.equal(f.kind, "transport");
	assert.equal(f.message, "Request timed out");
});
```

**Step 2: Run it, verify it fails**

Run: `npx tsx --test tests/failure.test.ts`
Expected: FAIL — `classifyFailure` is not exported.

**Step 3: Implement**

Append to `src/failure.ts`:

```ts
export type FailureKind = "cancelled" | "stalled" | "worker" | "transport";

export interface FailureContext {
	/** The prompt signal was aborted — user interrupt, superseded prompt, reload. */
	aborted: boolean;
	/** The failure came from the worker/bridge rather than the server. */
	workerFault?: boolean;
	/** The whole-prompt budget expired rather than a per-call server deadline. */
	budgetExceeded?: boolean;
}

export interface ClassifiedFailure {
	kind: FailureKind;
	/** Rendered cause, from {@linkcode describeCause}. */
	cause: string;
	/** Whether the shared connection should be dropped. */
	resetConnection: boolean;
}

/**
 * Attribute a rejection to the component that actually failed.
 *
 * The ordering is the point. `aborted` wins over everything because the SDK labels
 * an aborted request `REQUEST_TIMEOUT`, so the error text alone would report a
 * benign Esc as a dead server *and* reset a healthy session. `budgetExceeded`
 * comes next for the same reason one level up: with the per-call deadline owned by
 * the worker, the main-thread budget expiring says the prompt path was slow, not
 * that the server was.
 *
 * `resetConnection` is false for `cancelled` and `stalled` on purpose — in both
 * the connection is fine, and dropping it costs a fresh handshake on the next
 * prompt while fixing nothing.
 */
export function classifyFailure(
	err: unknown,
	ctx: FailureContext,
): ClassifiedFailure {
	const cause = err instanceof AlexandriaFailure ? err.message : describeCause(err);
	if (ctx.aborted) return { kind: "cancelled", cause, resetConnection: false };
	if (ctx.budgetExceeded) return { kind: "stalled", cause, resetConnection: false };
	if (ctx.workerFault) return { kind: "worker", cause, resetConnection: true };
	return { kind: "transport", cause, resetConnection: true };
}

/**
 * A rejection that already knows what it was.
 *
 * The tasks in `index.ts` classify at the throw site, where the signal state and
 * the bridge's fault flag are still in scope; `buildInjection` then only has to
 * read the result. Without this, the merge step would have to re-derive context it
 * cannot see — it receives a settled promise and nothing else.
 */
export class AlexandriaFailure extends Error {
	readonly kind: FailureKind;
	readonly resetConnection: boolean;
	constructor(
		message: string,
		opts: { kind: FailureKind; resetConnection: boolean },
	) {
		super(message);
		this.name = "AlexandriaFailure";
		this.kind = opts.kind;
		this.resetConnection = opts.resetConnection;
	}
}

/** Read the classification off a rejection, falling back to classifying it as a
 *  transport failure — the pre-existing behaviour, for any throw site not yet
 *  converted. */
export function failureOf(reason: unknown): ClassifiedFailure {
	if (reason instanceof AlexandriaFailure) {
		return {
			kind: reason.kind,
			cause: reason.message,
			resetConnection: reason.resetConnection,
		};
	}
	return classifyFailure(reason, { aborted: false });
}
```

**Step 4: Run it, verify it passes**

Run: `npx tsx --test tests/failure.test.ts`
Expected: PASS (9 tests).

**Step 5: Commit**

`git commit -am "fix(pi): classify prompt-path failures by responsible component"`

---

### Task 3: `buildInjection` consumes the classification

**Files:**
- Modify: `contrib/pi/extensions/alexandria/src/injection.ts`
- Test: `contrib/pi/extensions/alexandria/tests/injection.test.ts` (append)

**Step 1: Write the failing test**

Append to `tests/injection.test.ts` (reuse its existing `ok`/`bad`/`RECALL`/`REMINDERS` helpers):

```ts
const { AlexandriaFailure } = await import("../src/failure.js");

test("a cancelled prompt warns about nothing and keeps the session", () => {
	const out = buildInjection(
		bad(new AlexandriaFailure("This operation was aborted", {
			kind: "cancelled",
			resetConnection: false,
		})),
		ok(REMINDERS),
	);
	// Esc is not a server fault. The old code warned AND reset the client here.
	assert.equal(out.resetConnection ?? out.resetClient, false);
	assert.equal(
		out.notifications.some((n) => n.level === "warning"),
		false,
		JSON.stringify(out.notifications),
	);
	// The reminder half still injects and still announces.
	assert.equal(out.message?.content, "REMINDER-BLOCK");
});

test("a stall does not claim the server is unreachable", () => {
	const out = buildInjection(
		bad(new AlexandriaFailure("Alexandria prompt budget (10000 ms) exceeded", {
			kind: "stalled",
			resetConnection: false,
		})),
		ok(REMINDERS),
	);
	const warn = out.notifications.find((n) => n.level === "warning");
	assert.ok(warn, "a stall is still worth one warning");
	assert.doesNotMatch(warn.text, /unreachable/i);
	assert.match(warn.text, /budget|stall|slow/i);
	assert.equal(out.resetClient, false);
});

test("a transport failure names the OS cause, not 'fetch failed'", () => {
	const inner = Object.assign(new Error("connect ECONNREFUSED 127.0.0.1:3000"), {
		code: "ECONNREFUSED",
	});
	const out = buildInjection(
		bad(Object.assign(new TypeError("fetch failed"), { cause: inner })),
		bad(Object.assign(new TypeError("fetch failed"), { cause: inner })),
	);
	const warn = out.notifications.find((n) => n.level === "warning");
	assert.match(warn!.text, /ECONNREFUSED/);
	assert.equal(out.resetClient, true);
});

test("a worker fault is attributed to the memory client", () => {
	const out = buildInjection(
		bad(new AlexandriaFailure("worker exited before responding", {
			kind: "worker",
			resetConnection: true,
		})),
		ok(REMINDERS),
	);
	const warn = out.notifications.find((n) => n.level === "warning");
	assert.match(warn!.text, /memory client/i);
	assert.doesNotMatch(warn!.text, /unreachable/i);
	assert.equal(out.resetClient, true);
});
```

**Step 2: Run it, verify it fails**

Run: `npx tsx --test tests/injection.test.ts`
Expected: FAIL — cancelled case still warns and still sets `resetClient: true`.

**Step 3: Implement**

In `src/injection.ts`:
- Replace the local `reasonText` with `failureOf` from `./failure.js` (keep `reasonText` exported only if another module imports it — check with `rg -n "reasonText" src/`; if `index.ts` has its own duplicate copy, delete that one too, since two copies of the same helper is how they drifted apart).
- Rework the `failure` construction to switch on `kind`:
  - `cancelled` → `failure = null` (no warning at all).
  - `stalled` → `` `Alexandria prompt path exceeded its budget (${cause}); continuing without recall or reminders.` ``
  - `worker` → `` `Alexandria memory client failed (${cause}); continuing without recall or reminders.` ``
  - `transport` → keep the existing three-way shape (both-same-cause / both-distinct / recall-only), with `cause` from `failureOf`.
- `const resetClient = failure !== null && classified.resetConnection;` — i.e. compute it from the classification, not from "did anything fail". Rename the field to `resetConnection` in the `Injection` interface and update `index.ts`'s one call site.

**Rationale to carry into the code comment:** `resetClient` was `failure !== null`, which conflated "the server is gone" with "the user pressed Esc". Only the former warrants dropping a session.

**Step 4: Run it, verify it passes — including every pre-existing row**

Run: `npx tsx --test tests/injection.test.ts`
Expected: PASS, with no existing test edited. If an existing row asserted `resetClient: true` for a bare `new Error("timeout")`, that still holds (unclassified → transport).

**Step 5: Commit**

`git commit -am "fix(pi): stop reporting cancellation and stalls as an unreachable server"`

---

### Task 4: `worker-bridge` protocol — correlation and cancellation

**Files:**
- Create: `contrib/pi/extensions/alexandria/src/worker-bridge.ts`
- Test: `contrib/pi/extensions/alexandria/tests/worker-bridge.test.ts`

The bridge takes an injected `BridgeTransport` so the whole protocol is testable with no thread and no socket — the same discipline as `CallTool` in `recall.ts`.

**Step 1: Write the failing test**

```ts
import { test } from "node:test";
import assert from "node:assert/strict";

process.env.ALEXANDRIA_CLIENT_CONFIG ??= "/tmp/alexandria-tests-absent-client.toml";

const { createBridge } = await import("../src/worker-bridge.js");
type Wire = import("../src/worker-bridge.js").WorkerInbound;

/** A transport that records what was posted and lets the test drive replies. */
function fakeTransport() {
	const posted: Wire[] = [];
	let handler: ((m: unknown) => void) | null = null;
	let terminated = 0;
	return {
		posted,
		terminated: () => terminated,
		transport: {
			post: (m: Wire) => { posted.push(m); },
			onMessage: (h: (m: unknown) => void) => { handler = h; },
			onError: () => {},
			onExit: () => {},
			terminate: () => { terminated++; },
		},
		reply: (id: number, result: unknown) =>
			handler?.({ t: "result", id, ok: true, result }),
		fail: (id: number, message: string, code?: string) =>
			handler?.({ t: "result", id, ok: false, error: { message, code } }),
	};
}

test("requests are correlated by id, and results resolve the right caller", async () => {
	const f = fakeTransport();
	const b = createBridge(f.transport);
	const a = b.call("retrieve_memories", { q: 1 }, 5000);
	const c = b.call("check_reminders", {}, 5000);
	assert.equal(f.posted.length, 2);
	assert.notEqual(f.posted[0].id, f.posted[1].id);
	f.reply(f.posted[1].id!, { content: "second" });
	f.reply(f.posted[0].id!, { content: "first" });
	assert.deepEqual(await a, { content: "first" });
	assert.deepEqual(await c, { content: "second" });
});

test("an aborted signal posts a cancel for that id only", async () => {
	const f = fakeTransport();
	const b = createBridge(f.transport);
	const ac = new AbortController();
	const p = b.call("retrieve_memories", {}, 5000, ac.signal);
	ac.abort();
	const cancels = f.posted.filter((m) => m.t === "cancel");
	assert.equal(cancels.length, 1);
	assert.equal(cancels[0].id, f.posted[0].id);
	f.fail(cancels[0].id!, "This operation was aborted");
	await assert.rejects(p, /aborted/i);
});

test("a worker error rejects every pending call as a worker fault", async () => {
	let onError: ((e: Error) => void) | null = null;
	const posted: Wire[] = [];
	const b = createBridge({
		post: (m) => { posted.push(m); },
		onMessage: () => {},
		onError: (h) => { onError = h; },
		onExit: () => {},
		terminate: () => {},
	});
	const p1 = b.call("a", {}, 5000);
	const p2 = b.call("b", {}, 5000);
	onError!(new Error("worker crashed"));
	for (const p of [p1, p2]) {
		await assert.rejects(p, (e: Error & { workerFault?: boolean }) => {
			assert.equal(e.workerFault, true);
			return true;
		});
	}
});
```

**Step 2: Run it, verify it fails**

Run: `npx tsx --test tests/worker-bridge.test.ts`
Expected: FAIL — cannot find module `../src/worker-bridge.js`.

**Step 3: Implement**

`src/worker-bridge.ts`:

```ts
/**
 * The main-thread half of the worker protocol: id correlation, cancellation
 * forwarding, a watchdog, and crash recovery.
 *
 * Deliberately transport-injected. A real `Worker` cannot be exercised without a
 * socket on the other end, and this extension's tests are hermetic by rule — so
 * the protocol lives here against an interface, and `mcp-client.ts` supplies the
 * only production implementation. Same seam as `CallTool` in `recall.ts`.
 */

export interface WorkerInbound {
	t: "call" | "cancel" | "close";
	id?: number;
	name?: string;
	args?: Record<string, unknown>;
	timeoutMs?: number;
}

export interface BridgeTransport {
	post(msg: WorkerInbound): void;
	onMessage(handler: (msg: unknown) => void): void;
	onError(handler: (err: Error) => void): void;
	onExit(handler: (code: number | null) => void): void;
	terminate(): void;
}

/** Slack on top of the caller's own deadline before the watchdog gives up. The
 *  worker owns the real timeout; this only covers a worker that died without
 *  telling us, which must not be able to hang `before_agent_start` forever. */
const WATCHDOG_SLACK_MS = 2000;

export interface Bridge {
	call(
		name: string,
		args: Record<string, unknown>,
		timeoutMs?: number,
		signal?: AbortSignal,
	): Promise<unknown>;
	close(): void;
	/** True once the worker has failed and a respawn is needed. */
	readonly faulted: boolean;
}

export function createBridge(transport: BridgeTransport): Bridge {
	let nextId = 1;
	let faulted = false;
	const pending = new Map<
		number,
		{
			resolve: (v: unknown) => void;
			reject: (e: Error) => void;
			timer: ReturnType<typeof setTimeout> | undefined;
			onAbort: (() => void) | undefined;
		}
	>();

	function settle(id: number, err: Error | null, value?: unknown): void {
		const entry = pending.get(id);
		if (!entry) return;
		pending.delete(id);
		if (entry.timer !== undefined) clearTimeout(entry.timer);
		if (entry.onAbort) entry.signal?.removeEventListener?.("abort", entry.onAbort);
		if (err) entry.reject(err);
		else entry.resolve(value);
	}

	function failAll(err: Error): void {
		faulted = true;
		for (const id of [...pending.keys()]) settle(id, err);
	}

	transport.onMessage((raw) => {
		const msg = raw as { t?: string; id?: number; ok?: boolean; result?: unknown; error?: { message?: string; code?: string } };
		if (msg?.t !== "result" || typeof msg.id !== "number") return;
		if (msg.ok) {
			settle(msg.id, null, msg.result);
			return;
		}
		const e = new Error(msg.error?.message ?? "worker reported an unknown error") as Error & {
			code?: string;
			workerFault?: boolean;
		};
		if (msg.error?.code) e.code = msg.error.code;
		settle(msg.id, e);
	});

	// A dead worker must reject what is outstanding, not leave the prompt hanging.
	transport.onError((err) => failAll(Object.assign(new Error(`memory worker failed: ${err.message}`), { workerFault: true })));
	transport.onExit(() =>
		failAll(Object.assign(new Error("memory worker exited before responding"), { workerFault: true })),
	);

	return {
		get faulted() {
			return faulted;
		},
		call(name, args, timeoutMs, signal) {
			if (faulted) {
				return Promise.reject(
					Object.assign(new Error("memory worker is not running"), { workerFault: true }),
				);
			}
			const id = nextId++;
			return new Promise<unknown>((resolve, reject) => {
				const timer =
					timeoutMs === undefined
						? undefined
						: setTimeout(() => {
								settle(
									id,
									Object.assign(new Error(`memory worker did not respond within ${timeoutMs + WATCHDOG_SLACK_MS}ms`), {
										workerFault: true,
									}),
								);
								transport.post({ t: "cancel", id });
							}, timeoutMs + WATCHDOG_SLACK_MS);

				let onAbort: (() => void) | undefined;
				if (signal) {
					if (signal.aborted) {
						if (timer !== undefined) clearTimeout(timer);
						reject(new Error("This operation was aborted"));
						transport.post({ t: "cancel", id });
						return;
					}
					onAbort = () => {
						transport.post({ t: "cancel", id });
						// The worker rejects the call; do not pre-empt it here, or the
						// classification cannot tell a cancel from a crash.
					};
					signal.addEventListener("abort", onAbort, { once: true });
				}

				pending.set(id, { resolve, reject, timer, onAbort });
				transport.post({ t: "call", id, name, args, ...(timeoutMs === undefined ? {} : { timeoutMs }) });
			});
		},
		close() {
			transport.post({ t: "close" });
			for (const id of [...pending.keys()]) {
				settle(id, Object.assign(new Error("memory worker closed"), { workerFault: true }));
			}
			transport.terminate();
		},
	};
}
```

**Note:** `settle` references `entry.signal` in the snippet above — store the `signal` on the pending entry (or drop the `removeEventListener` and rely on `{ once: true }`). Prefer dropping it: `{ once: true }` already unregisters, and keeping a `signal` reference per pending call is state with no reader. Fix during implementation and keep the test asserting only observable behaviour.

**Step 4: Run it, verify it passes**

Run: `npx tsx --test tests/worker-bridge.test.ts`
Expected: PASS (3 tests).

**Step 5: Commit**

`git add -A && git commit -m "feat(pi): add a transport-injected worker bridge for the MCP protocol"`

---

### Task 5: Bridge watchdog and retry-once on worker fault

**Files:**
- Modify: `contrib/pi/extensions/alexandria/src/worker-bridge.ts`
- Test: `contrib/pi/extensions/alexandria/tests/worker-bridge.test.ts` (append)

**Step 1: Write the failing test**

```ts
test("the watchdog fires when a worker never answers, flagged as a worker fault", async () => {
	const f = fakeTransport();
	const b = createBridge(f.transport);
	// Timeout plus slack; use fake timers or a short timeout to keep the suite fast.
	const p = b.call("retrieve_memories", {}, 5);
	await assert.rejects(p, (e: Error & { workerFault?: boolean }) => {
		assert.equal(e.workerFault, true);
		return true;
	});
	// It must also have told the worker to stop working on it.
	assert.ok(f.posted.some((m) => m.t === "cancel"));
});

test("after a fault the bridge refuses new work rather than queueing into a void", async () => {
	const f = fakeTransport();
	const b = createBridge(f.transport);
	b.close();
	assert.equal(b.faulted, true);
	await assert.rejects(b.call("retrieve_memories", {}, 5000), /not running/i);
});
```

**Step 2: Run it, verify it fails**, then **Step 3** make `close()` set `faulted = true` (a closed bridge must not silently accept work) and confirm the watchdog already rejects with `workerFault`.

**Step 4: Run the whole suite**

Run: `npm test`
Expected: PASS.

**Step 5: Commit**

`git commit -am "fix(pi): bound a wedged memory worker and refuse work after a fault"`

---

### Task 6: The worker entry point

**Files:**
- Create: `contrib/pi/extensions/alexandria/src/mcp-worker.ts`

No new test file: this module's only logic is wiring the SDK to the protocol, and the protocol is covered by Task 4/5. It is exercised end-to-end in Task 8 against a real server, and `npm run typecheck` is the gate here.

**Step 1: Implement**

Move the existing `connect()`, `getClient()`, stale-session detection and reconnect-once retry from `mcp-client.ts` **into** `mcp-worker.ts`, unchanged in behaviour — including the rule that a failed connect must not stay cached, and `HANDSHAKE_TIMEOUT_MS = 5000`. The worker owns the `Client`, the `StreamableHTTPClientTransport`, and therefore every SDK timer.

```ts
/**
 * Worker entry point: owns the MCP Client, its Streamable HTTP session, and every
 * SDK deadline.
 *
 * The reason this is a thread and not a module: the SDK implements its per-request
 * timeout as a `setTimeout` on whatever loop calls it. On pi's main loop that loop
 * is shared with every other in-process extension, and a synchronous block longer
 * than the remaining budget makes the overdue timer win the race against a response
 * that has already arrived — reporting a healthy ~45 ms server as
 * `REQUEST_TIMEOUT`. Here the timers run on a loop nothing else can block.
 *
 * `serverUrl` arrives via workerData rather than being re-read from CONFIG, so the
 * worker cannot drift from the URL the main thread validated and logged.
 */
import { parentPort, workerData } from "node:worker_threads";
// ... Client, StreamableHTTPClientTransport, connect(), getClient(),
// isStaleSessionError(), and the reconnect-once retry, moved verbatim.

const SERVER_URL: string = workerData.serverUrl;

parentPort!.on("message", async (msg) => { /* call | cancel | close */ });
```

Reply shape must match the bridge: `{t:"result", id, ok:true, result}` or `{t:"result", id, ok:false, error:{message, code}}`. Serialise the error to `{message, code}` only — a thrown `Error` does not survive `postMessage` with its prototype, and the bridge reconstructs it.

Per-call `AbortController` map inside the worker so `{t:"cancel", id}` aborts exactly that call.

**Step 2: Typecheck**

Run: `npm run typecheck`
Expected: clean.

**Step 3: Commit**

`git add -A && git commit -m "feat(pi): move the MCP client and its deadlines onto a worker thread"`

---

### Task 7: `mcp-client.ts` becomes the proxy, signatures preserved

**Files:**
- Modify: `contrib/pi/extensions/alexandria/src/mcp-client.ts`
- Modify: `contrib/pi/extensions/alexandria/src/index.ts` (rename `resetClient` → `resetConnection`)

**Step 1: Implement**

Keep these exports byte-compatible in signature, because `recall.ts`, `reminders.ts` and their tests depend on them:
`PROMPT_CALL_TIMEOUT_MS`, `PROMPT_BUDGET_MS`, `CallTool`, `callToolWithRetry(name, args, timeoutMs?, signal?)`, `extractTextContent`, `toolErrorMessage`, `storeMemory`.

Replace the client cache with: lazy worker spawn (`workerData: { serverUrl: CONFIG.serverUrl }`), a bridge over it, and **retry-once on `workerFault` only**:

```ts
/**
 * Retry exactly once, and only when the *worker* failed — never when the server
 * was slow.
 *
 * With the SDK deadline on the worker's loop, a main-loop stall no longer produces
 * a timeout at all: the late response is simply used when the loop resumes. So the
 * only recoverable fault left is a worker that died or wedged, where respawning is
 * a real fix. Retrying a genuine server timeout would just double the wait on a
 * server that is already struggling.
 */
async function callOnce(name, args, timeoutMs, signal, allowRetry: boolean) { ... }
```

`resetConnection()` → terminate the worker and clear the handle so the next call respawns (preserving the current "do not leak a live session per failure" property). `closeClient()` → bridge `close()` with a bounded wait, then `terminate()`.

Worker startup: spawn **eagerly but not awaited** from the extension's existing `if (!CONFIG.recallDisabled || !CONFIG.remindersDisabled)` gate, so module load overlaps pi's startup instead of being paid by the first prompt.

**Step 2: Typecheck + existing suite**

Run: `npm run typecheck && npm test`
Expected: clean, and **no existing test file edited** — that is the check that the seam held.

**Step 3: Commit**

`git commit -am "refactor(pi): proxy the MCP client through the worker bridge"`

---

### Task 8: End-to-end verification against a real server

**Files:**
- Test (manual, not committed to the suite — the suite is hermetic by rule): the harnesses in `/tmp/alex-diag/`

**Step 1:** Re-run `loop-race.ts` against the rebuilt extension. Expected: the 6s freeze no longer produces `REQUEST_TIMEOUT`; the call resolves with its memories after the loop resumes.

**Step 2:** Re-run `failure-modes.ts`. Expected: closed port now reads `connect ECONNREFUSED 127.0.0.1:3999 [ECONNREFUSED]`, NXDOMAIN reads `getaddrinfo ENOTFOUND ... [ENOTFOUND]`, blackhole still reads `Request timed out` — and the blackhole case is now a *true* positive.

**Step 3:** Confirm no regression in the happy path: `sweep.ts` p50/max within noise of the 45ms/116ms baseline, and worker startup does not add measurable cost to the first prompt.

**Step 4:** Verify the abort path: interrupt a prompt mid-recall and confirm **no warning** appears and the session is **not** reset.

---

### Task 9: Documentation

**Files:**
- Modify: `contrib/pi/extensions/alexandria/README.md` (the budget paragraph, ~lines 99-101)
- Modify: `AGENTS.md` (Non-Obvious Patterns)
- Modify: `contrib/pi/README.md` if it restates the budget

**Step 1:** The README's budget paragraph is now wrong in a way that matters: `PROMPT_CALL_TIMEOUT_MS` measures **real server time on the worker's loop**, while `PROMPT_BUDGET_MS` is a **main-thread delivery deadline**. Say so, and say why the distinction exists (the reproduced race), because a future reader will otherwise "simplify" the worker away.

**Step 2:** Add an AGENTS.md bullet under Non-Obvious Patterns covering: the worker boundary and its reason; that `worker-bridge.ts` is transport-injected so the protocol is testable hermetically; that retry is worker-fault-only by design; and that `resetConnection` is deliberately *not* called for `cancelled`/`stalled`.

**Step 3:** Per AGENTS.md, `.github/workflows/ci.yml` does not call `just ci` — confirm the `Pi companion` job still runs `npm run typecheck` + `npm test` and needs no change. No Rust files are touched, so `just test` is unaffected; run `just ext-test` as the gate.

**Step 4: Commit**

`git commit -am "docs(pi): record the worker boundary and the corrected budget semantics"`

---

## Out of scope (explicitly)

- **Raising `PROMPT_CALL_TIMEOUT_MS` / `PROMPT_BUDGET_MS`.** Rejected: it hides the attribution bug and adds seconds to every genuinely-dead prompt.
- **A loop-drift sentinel.** Declined by the operator. Consequence worth restating: after this change a >5s main-loop block no longer harms recall, but it still freezes the whole turn and becomes invisible. If turn-level stalls need diagnosing later, that instrumentation is the missing piece.
- **The `relativity.grv.st` DNS split-horizon issue** affecting `mars` / `askrabec-mac` (no public A record; resolves only via LAN/NetBird DNS to `192.168.160.1`, while the server also answers on `100.91.177.24:3000`). Real and latent, unrelated to this bug, and a dotfiles change rather than a code change.
- **`embedding.max_tokens` = 128 truncation**, observed live while storing these findings (`truncated: true`). Already documented in `docs/performance-and-ability-findings.md`.
