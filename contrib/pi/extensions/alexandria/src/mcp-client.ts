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

let clientPromise: Promise<Client> | null = null;

/**
 * Budget for the connect handshake: a single cheap round trip, so a server that
 * has not answered within this is not going to answer anything else either. The
 * SDK default is 60 s, which would let a socket-accepting but unresponsive
 * server stall the prompt path for a minute before a tool call was even made.
 */
const HANDSHAKE_TIMEOUT_MS = 5000;

/**
 * Budget for the calls made on the prompt path (`retrieve_memories`,
 * `check_reminders`). The SDK waits 60 s per request by default; both injections
 * block the turn and both are best-effort, so they give up early and warn
 * instead. Store calls keep the SDK default — they are fire-and-forget, and a
 * store that embeds a long document legitimately takes a while.
 */
export const PROMPT_CALL_TIMEOUT_MS = 5000;

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

export function resetClient(): void {
	clientPromise = null;
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
 * for why the prompt path passes one.
 */
export async function callToolWithRetry(
	name: string,
	args: Record<string, unknown>,
	timeoutMs?: number,
): Promise<Awaited<ReturnType<Client["callTool"]>>> {
	const options = timeoutMs === undefined ? {} : { timeout: timeoutMs };
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

export async function storeMemory(
	content: string,
	tags: string[],
): Promise<void> {
	await callToolWithRetry("store_memory", { content, tags });
}
