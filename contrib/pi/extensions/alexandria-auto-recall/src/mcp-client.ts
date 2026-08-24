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

export async function getClient(): Promise<Client> {
	if (!clientPromise) {
		clientPromise = (async () => {
			let url: URL;
			try {
				url = new URL(CONFIG.serverUrl);
			} catch {
				throw new Error(`Invalid ALEXANDRIA_URL: ${CONFIG.serverUrl}`);
			}
			const client = new Client({
				name: "alexandria-auto-recall",
				version: "2.0.0",
			});
			const transport = new StreamableHTTPClientTransport(url);
			await client.connect(transport);
			return client;
		})();
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
 */
export async function callToolWithRetry(
	name: string,
	args: Record<string, unknown>,
): Promise<Awaited<ReturnType<Client["callTool"]>>> {
	try {
		const client = await getClient();
		return await client.callTool({ name, arguments: args });
	} catch (err) {
		if (isStaleSessionError(err)) {
			resetClient();
			const client = await getClient();
			return await client.callTool({ name, arguments: args });
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
