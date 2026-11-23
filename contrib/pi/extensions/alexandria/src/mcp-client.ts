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
import { createHash } from "node:crypto";
import { CONFIG } from "./config.js";

let clientPromise: Promise<Client> | null = null;
let clientIdentity: string | null = null;

const UPGRADE_GUIDANCE =
	"Alexandria server is incompatible: upgrade it to a build with resolve_project support and bearer authentication.";

/** Headers for this companion only; pi's native MCP client has its own transport/configuration. */
export function authorizationHeaders(token = CONFIG.apiToken): Record<string, string> {
	if (!token) {
		throw new Error(`${UPGRADE_GUIDANCE} Configure ALEXANDRIA_API_TOKEN for the companion.`);
	}
	return { Authorization: `Bearer ${token}` };
}

export function assertProjectResolutionSupported(toolNames: string[]): void {
	if (!toolNames.includes("resolve_project")) throw new Error(UPGRADE_GUIDANCE);
}

function credentialKey(token: string | undefined): string {
	return token ? createHash("sha256").update(token).digest("hex") : "missing-token";
}

function identityKey(server = CONFIG.serverUrl, account = credentialKey(CONFIG.apiToken)): string {
	return `${server}\u0000${account}`;
}

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
 * instead. Explicit store_memory calls use the configured/native MCP path, not
 * this automatic prompt dispatcher.
 */
export const PROMPT_CALL_TIMEOUT_MS = CONFIG.promptCallTimeoutMs;

/**
 * Ceiling on the whole per-prompt path, as one abort deadline.
 *
 * The per-call budget above is exactly that — per call — and the prompt path
 * stacks several: a git probe (2 s), a handshake (5 s) and tool calls, with one
 * reconnect retry on top. Pi waits for `before_agent_start` before the turn starts,
 * so the wait a user experiences is bounded by this shared deadline, not the sum of
 * each stage's individual timeout.
 */
export const PROMPT_BUDGET_MS = CONFIG.promptBudgetMs;

async function connect(): Promise<Client> {
	let url: URL;
	try {
		url = new URL(CONFIG.serverUrl);
	} catch {
		throw new Error("Invalid ALEXANDRIA_URL format");
	}
	const client = new Client({
		name: "alexandria",
		version: "2.1.0",
	});
	const transport = new StreamableHTTPClientTransport(url, {
		requestInit: { headers: authorizationHeaders() },
	});
	try {
		await client.connect(transport, { timeout: HANDSHAKE_TIMEOUT_MS });
		const { tools } = await client.listTools();
		assertProjectResolutionSupported(tools.map((tool) => tool.name));
	} catch (err) {
		if (/401|unauthori[sz]ed/i.test(err instanceof Error ? err.message : String(err))) {
			throw new Error(`${UPGRADE_GUIDANCE} Check ALEXANDRIA_API_TOKEN and the server auth configuration.`);
		}
		throw err;
	}
	connectionGeneration++;
	return client;
}

function awaitWithSignal<T>(promise: Promise<T>, signal?: AbortSignal): Promise<T> {
	if (!signal) return promise;
	if (signal.aborted) return Promise.reject(signal.reason ?? new Error("Prompt path aborted"));
	return new Promise<T>((resolve, reject) => {
		const onAbort = () => {
			signal.removeEventListener("abort", onAbort);
			reject(signal.reason ?? new Error("Prompt path aborted"));
		};
		const settle = (fn: (value: T) => void, value: T) => {
			signal.removeEventListener("abort", onAbort);
			fn(value);
		};
		signal.addEventListener("abort", onAbort, { once: true });
		promise.then(
			(value) => settle(resolve, value),
			(error) => {
				signal.removeEventListener("abort", onAbort);
				reject(error);
			},
		);
	});
}

export async function getClient(signal?: AbortSignal): Promise<Client> {
	const identity = identityKey();
	if (clientIdentity !== null && clientIdentity !== identity) resetClient();
	clientIdentity = identity;
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
	return awaitWithSignal(clientPromise, signal);
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
	clientIdentity = null;
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
export function callToolWithRetry(
	name: string,
	args: Record<string, unknown>,
	timeoutMs?: number,
	signal?: AbortSignal,
): Promise<Awaited<ReturnType<Client["callTool"]>>> {
	return callToolForIdentity(identityKey(), name, args, timeoutMs, signal);
}

async function callToolForIdentity(
	identity: string,
	name: string,
	args: Record<string, unknown>,
	timeoutMs?: number,
	signal?: AbortSignal,
): Promise<Awaited<ReturnType<Client["callTool"]>>> {
	const assertIdentity = () => {
		if (identityKey() !== identity) {
			throw new Error("Alexandria companion credentials or endpoint changed. Retry with the current configuration.");
		}
	};
	const options = {
		...(timeoutMs === undefined ? {} : { timeout: timeoutMs }),
		...(signal === undefined ? {} : { signal }),
	};
	const attempt = async () => {
		assertIdentity();
		const client = await getClient(signal);
		// A cold handshake yields control. Refuse dispatch if the snapshot
		// no longer matches, even though the resolved client retained its headers.
		assertIdentity();
		return client.callTool({ name, arguments: args }, options);
	};
	try {
		return await attempt();
	} catch (err) {
		if (!isStaleSessionError(err) || signal?.aborted) throw err;
		assertIdentity();
		resetClient();
		return attempt();
	}
}

export interface AuthenticatedServer {
	serverKey: string;
	accountKey: string;
	callTool: CallTool;
}

/** Identity and call surface shared with context resolution, never the bearer secret itself. */
export function authenticatedServer(): AuthenticatedServer {
	const token = CONFIG.apiToken;
	if (!token) authorizationHeaders(token);
	const serverKey = CONFIG.serverUrl;
	const accountKey = credentialKey(token);
	const identity = identityKey(serverKey, accountKey);
	return { serverKey, accountKey, callTool: (...args) => callToolForIdentity(identity, ...args) };
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
