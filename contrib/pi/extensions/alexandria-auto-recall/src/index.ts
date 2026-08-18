/**
 * Alexandria Auto-Recall Extension
 *
 * On every user prompt, proactively queries the Alexandria memory MCP server
 * (retrieve_memories) using the prompt text and injects any hits above a
 * similarity threshold into context as a message before the agent loop
 * starts. This is the "Tier 3" aggressive nudge: the agent never has to
 * decide to check memory, relevant memories are just already there.
 *
 * Tradeoffs (why this is opt-in rather than the default nudge mechanism):
 * - Adds one extra HTTP round-trip + embedding call to every turn's latency.
 * - Injects content the agent didn't ask for; if retrieval quality is poor
 *   (still v0.2 clustering) this is noise, not signal.
 * - Harder to reason about than the SKILL.md-driven proactive tool calls —
 *   memory injection becomes invisible plumbing instead of a visible tool
 *   call in the transcript.
 *
 * Config (env vars, all optional):
 *   ALEXANDRIA_URL                          default: http://127.0.0.1:3000/mcp
 *   ALEXANDRIA_AUTO_RECALL_LIMIT             default: 5
 *   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY    default: 0.5
 *
 * Disable per-session with: ALEXANDRIA_AUTO_RECALL=off
 */

import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const SERVER_URL = process.env.ALEXANDRIA_URL ?? "http://127.0.0.1:3000/mcp";
const RESULT_LIMIT = Number(process.env.ALEXANDRIA_AUTO_RECALL_LIMIT ?? "5");
const MIN_SIMILARITY = Number(process.env.ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY ?? "0.5");
const DISABLED = process.env.ALEXANDRIA_AUTO_RECALL === "off";

interface RetrievedMemory {
	id: string;
	content: string;
	similarity: number;
	tags?: string[];
}

interface RetrieveMemoriesResponse {
	results?: RetrievedMemory[];
	error?: string;
}

/** Lazily-connected, session-lifetime client. Reset on failure rather than caching a dead connection. */
let clientPromise: Promise<Client> | null = null;

async function getClient(): Promise<Client> {
	if (!clientPromise) {
		clientPromise = (async () => {
			const client = new Client({ name: "alexandria-auto-recall", version: "1.0.0" });
			const transport = new StreamableHTTPClientTransport(new URL(SERVER_URL));
			await client.connect(transport);
			return client;
		})();
	}
	return clientPromise;
}

function resetClient(): void {
	clientPromise = null;
}

function extractTextContent(content: unknown): string | undefined {
	if (!Array.isArray(content)) return undefined;
	for (const block of content) {
		if (block && typeof block === "object" && "type" in block && block.type === "text" && "text" in block) {
			return String((block as { text: unknown }).text);
		}
	}
	return undefined;
}

async function retrieveMemories(query: string): Promise<RetrievedMemory[]> {
	const client = await getClient();
	const result = await client.callTool({
		name: "retrieve_memories",
		arguments: { query, limit: RESULT_LIMIT },
	});

	const text = extractTextContent(result.content);
	if (!text) return [];

	let parsed: RetrieveMemoriesResponse;
	try {
		parsed = JSON.parse(text) as RetrieveMemoriesResponse;
	} catch {
		return [];
	}

	return (parsed.results ?? []).filter((m) => m.similarity >= MIN_SIMILARITY);
}

function formatMemoriesBlock(memories: RetrievedMemory[]): string {
	const lines = memories.map((m) => {
		const tags = m.tags && m.tags.length > 0 ? ` [${m.tags.join(", ")}]` : "";
		return `- (similarity ${m.similarity.toFixed(2)}, id ${m.id})${tags} ${m.content}`;
	});
	return [
		"Relevant memories retrieved automatically from Alexandria for this prompt:",
		...lines,
		"",
		"These are surfaced proactively; verify relevance before relying on them, and use update_memory if any is stale.",
	].join("\n");
}

export default function alexandriaAutoRecall(pi: ExtensionAPI) {
	if (DISABLED) return;

	pi.on("before_agent_start", async (event, ctx) => {
		const query = event.prompt?.trim();
		if (!query) return;

		try {
			const memories = await retrieveMemories(query);
			if (memories.length === 0) return;

			return {
				message: {
					customType: "alexandria-auto-recall",
					content: formatMemoriesBlock(memories),
					display: true,
				},
			};
		} catch (err) {
			// Fail open: never block the agent turn on memory-server issues.
			resetClient();
			ctx.ui.notify(
				`Alexandria auto-recall failed (${err instanceof Error ? err.message : String(err)}); continuing without it.`,
				"warning",
			);
			return;
		}
	});

	pi.on("session_shutdown", async () => {
		if (clientPromise) {
			try {
				const client = await clientPromise;
				await client.close();
			} catch {
				// best-effort cleanup
			}
			clientPromise = null;
		}
	});
}
