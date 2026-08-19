/**
 * Shared MCP client for communicating with the Alexandria memory server.
 * Lazily connects on first use, resets on failure, closes on shutdown.
 */

import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";

const SERVER_URL = process.env.ALEXANDRIA_URL ?? "http://127.0.0.1:3000/mcp";

let clientPromise: Promise<Client> | null = null;

export async function getClient(): Promise<Client> {
	if (!clientPromise) {
		clientPromise = (async () => {
			let url: URL;
			try {
				url = new URL(SERVER_URL);
			} catch {
				throw new Error(`Invalid ALEXANDRIA_URL: ${SERVER_URL}`);
			}
			const client = new Client({ name: "alexandria-auto-recall", version: "2.0.0" });
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

export function extractTextContent(content: unknown): string | undefined {
	if (!Array.isArray(content)) return undefined;
	for (const block of content) {
		if (block && typeof block === "object" && "type" in block && block.type === "text" && "text" in block) {
			return String((block as { text: unknown }).text);
		}
	}
	return undefined;
}

export async function storeMemory(content: string, tags: string[]): Promise<void> {
	const client = await getClient();
	await client.callTool({
		name: "store_memory",
		arguments: { content, tags },
	});
}
