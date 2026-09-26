/**
 * Shared MCP client for communicating with the Alexandria memory server.
 * Lazily connects on first use, resets on stale session, closes on shutdown.
 *
 * Handles stale Streamable HTTP sessions transparently: if the server returns
 * "Session not found" (e.g. after a server restart), the client reconnects
 * and retries the operation once before propagating the error.
 */

import {
	Client,
	StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import { CONFIG } from "./config.js";
import type { SessionArgs } from "./session-args.js";

let clientPromise: Promise<Client> | null = null;

/**
 * Budget for the connect handshake: a single cheap round trip, so a server that
 * has not answered within this is not going to answer anything else either. The
 * SDK default is 60 s, which would let a socket-accepting but unresponsive
 * server stall the prompt path for a minute before a tool call was even made.
 */
const HANDSHAKE_TIMEOUT_MS = 5000;

/**
 * How many connections this process has established, monotonically.
 *
 * The prompt path snapshots this before and after so the diagnostic log can record
 * whether a run paid a handshake. That matters because the cold handshake is the
 * only window measured to lose the race against a stalled event loop: freezing pi's
 * loop for 6 s mid-handshake produced a false `REQUEST_TIMEOUT` in 6 of 6 fresh
 * processes, while the same freeze against an already-connected client resolved
 * normally in 15 of 15. A false timeout that only happens on cold connects is a
 * different problem from one that can happen on any call, and the log should be able
 * to tell them apart.
 */
let connectionGeneration = 0;

/** Connections established so far. See {@linkcode connectionGeneration}. */
export function connectionsEstablished(): number {
	return connectionGeneration;
}

/**
 * Budget for the calls made on the prompt path (`retrieve_memories`,
 * `check_reminders`). The SDK waits 60 s per request by default; both injections
 * block the turn and both are best-effort, so they give up early and warn
 * instead. Store calls keep the SDK default — they are fire-and-forget, and a
 * store that embeds a long document legitimately takes a while.
 */
export const PROMPT_CALL_TIMEOUT_MS = 5000;

/**
 * Ceiling on the whole per-prompt path, as one abort deadline.
 *
 * The per-call budget above is exactly that — per call — and the prompt path
 * stacks several: a git probe (2 s), a handshake (5 s) and a call (5 s), with one
 * reconnect retry on top. Pi waits for `before_agent_start` before the turn
 * starts, so the wait a user actually experiences is the sum, not the 5 s a single
 * constant suggests. This is the number to quote in the docs.
 */
export const PROMPT_BUDGET_MS = 10_000;

async function connect(): Promise<Client> {
	let url: URL;
	try {
		url = new URL(CONFIG.serverUrl);
	} catch {
		throw new Error(`Invalid ALEXANDRIA_URL: ${CONFIG.serverUrl}`);
	}
	const client = new Client({
		name: "alexandria",
		version: "2.1.0",
	});
	const transport = new StreamableHTTPClientTransport(url);
	await client.connect(transport, { timeout: HANDSHAKE_TIMEOUT_MS });
	connectionGeneration++;
	return client;
}

export async function getClient(): Promise<Client> {
	if (!clientPromise) {
		const attempt = connect();
		clientPromise = attempt;
		// A failed connect must not stay cached. Without this, one unreachable
		// server (not up yet, bad ALEXANDRIA_URL, a network blip) replays the same
		// rejection to every later caller until the process restarts, so per-prompt
		// work like reminder delivery never resumes when the server comes back.
		// The guard keeps the clear from clobbering a newer attempt.
		attempt.catch(() => {
			if (clientPromise === attempt) clientPromise = null;
		});
	}
	return clientPromise;
}

/**
 * Open the connection during extension load instead of on first use.
 *
 * Not an optimisation, though it does remove the handshake from the first prompt of
 * a session. It is the fix for the one measured false-timeout window: the SDK arms
 * its handshake deadline as a `setTimeout` on pi's main loop, which every other
 * in-process extension shares, so a stall of more than the remaining budget makes
 * the overdue timer win the race against a handshake that was succeeding. Moving the
 * handshake off the prompt path means nothing is in flight to lose that race when a
 * prompt starts.
 *
 * Deliberately not awaited — extension load must not block on the server — and a
 * failure is ignored because `getClient()` already refuses to cache a failed connect,
 * so the first real call simply tries again. Pre-warming can therefore never make
 * things worse than not pre-warming.
 *
 * Does not cover a reconnect after `resetClient()` (server restart, transport
 * failure); those still handshake on the prompt path. Cancelling a prompt no longer
 * causes one, which is what made this gap small enough to accept.
 */
export function prewarm(): void {
	void getClient().catch(() => {
		/* a cold start against a server that is not up yet is normal, not an error */
	});
}

/**
 * Drop the cached client so the next call reconnects.
 *
 * The client being dropped is also closed: nulling the promise alone leaks a live
 * client and its Streamable HTTP session on every timeout, and the prompt path
 * resets once per failed prompt — so a server that hangs for an afternoon would
 * accumulate one orphaned session per prompt.
 */
export function resetClient(): void {
	const dropped = clientPromise;
	clientPromise = null;
	if (dropped) {
		void dropped
			.then((client) => client.close())
			.catch(() => {
				/* best-effort: the connection is already being abandoned */
			});
	}
}

export async function closeClient(): Promise<void> {
	if (clientPromise) {
		try {
			const client = await clientPromise;
			await client.close();
		} catch {
			// best-effort cleanup
		}
		clientPromise = null;
	}
}

/** The transport the prompt-path modules speak through, as a type so they can take
 *  it as a parameter: reaching `callToolWithRetry` from a test means module mocking,
 *  which hangs under tsx — and the payload contract is exactly what needs covering. */
export type CallTool = (
	name: string,
	args: Record<string, unknown>,
	timeoutMs?: number,
	signal?: AbortSignal,
) => Promise<{ content?: unknown; isError?: boolean }>;

/** Check if an error is a stale Streamable HTTP session (server restart, expiry, etc.) */
function isStaleSessionError(err: unknown): boolean {
	if (!(err instanceof Error)) return false;
	const msg = err.message.toLowerCase();
	return msg.includes("session not found") || msg.includes("session_not_found");
}

/**
 * Call an MCP tool with automatic reconnect on stale session.
 * If the first attempt fails with "Session not found", resets the client,
 * establishes a fresh connection, and retries exactly once.
 *
 * `timeoutMs` bounds the wait for this call; see {@linkcode PROMPT_CALL_TIMEOUT_MS}
 * for why the prompt path passes one. `signal` cancels the in-flight request: a
 * prompt the user has abandoned (or that has hit the dispatcher's overall budget)
 * should stop spending a socket rather than run to its own timeout.
 */
export async function callToolWithRetry(
	name: string,
	args: Record<string, unknown>,
	timeoutMs?: number,
	signal?: AbortSignal,
): Promise<Awaited<ReturnType<Client["callTool"]>>> {
	const options = {
		...(timeoutMs === undefined ? {} : { timeout: timeoutMs }),
		...(signal === undefined ? {} : { signal }),
	};
	try {
		const client = await getClient();
		return await client.callTool({ name, arguments: args }, options);
	} catch (err) {
		if (isStaleSessionError(err)) {
			resetClient();
			const client = await getClient();
			return await client.callTool({ name, arguments: args }, options);
		}
		throw err;
	}
}

export function extractTextContent(content: unknown): string | undefined {
	if (!Array.isArray(content)) return undefined;
	for (const block of content) {
		if (
			block &&
			typeof block === "object" &&
			"type" in block &&
			block.type === "text" &&
			"text" in block
		) {
			return String((block as { text: unknown }).text);
		}
	}
	return undefined;
}

/**
 * The message of a failed tool result, or undefined when it succeeded. The server reports a
 * rejected call as a normal result flagged `isError` with a `{"status":"error","message":...}`
 * text block, not as a thrown error, so a caller that does not look never learns of it. `isError`
 * alone decides; the body only supplies the wording.
 */
export function toolErrorMessage(result: { content?: unknown; isError?: boolean }): string | undefined {
	if (result.isError !== true) return undefined;
	const text = extractTextContent(result.content);
	if (text === undefined) return "tool call failed";
	try {
		const message = (JSON.parse(text) as { message?: unknown } | null)?.message;
		if (typeof message === "string") return message;
	} catch {
		/* not JSON: the text is the message */
	}
	return text;
}

export async function storeMemory(
	content: string,
	tags: string[],
	session: SessionArgs,
): Promise<void> {
	const result = await callToolWithRetry("store_memory", { content, tags, ...session });
	const err = toolErrorMessage(result);
	if (err) throw new Error(err);
}
