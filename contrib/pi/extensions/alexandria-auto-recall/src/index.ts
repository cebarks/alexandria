/**
 * Alexandria Auto-Recall & Auto-Store Extension
 *
 * Recall (before_agent_start):
 *   Queries Alexandria for memories relevant to the user's prompt and injects
 *   them into context before the agent starts. Disable: ALEXANDRIA_AUTO_RECALL=off
 *
 * Store (heuristic detectors + LLM extraction):
 *   - Correction/preference detectors fire on user prompts (before_agent_start)
 *   - Error resolution tracker pairs errors with fixes (tool_execution_end → agent_end)
 *   - Tool dedup tracker records agent-initiated store_memory calls (tool_result)
 *   - LLM extraction at session_shutdown extracts remaining durable facts
 *   Disable: ALEXANDRIA_AUTO_STORE=off
 *
 * Config (env vars, all optional):
 *   ALEXANDRIA_URL                          default: http://127.0.0.1:3000/mcp
 *   ALEXANDRIA_AUTO_RECALL                  set to "off" to disable recall
 *   ALEXANDRIA_AUTO_RECALL_LIMIT            default: 5
 *   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY   default: 0.5
 *   ALEXANDRIA_AUTO_STORE                   set to "off" to disable all store behavior
 *   ALEXANDRIA_EXTRACT_MODEL                default: vertex/claude-haiku-4-5
 *   ALEXANDRIA_EXTRACT_TIMEOUT_MS           default: 5000
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { resetClient, closeClient, extractTextContent, getClient } from "./mcp-client.js";

const RECALL_DISABLED = process.env.ALEXANDRIA_AUTO_RECALL === "off";
const RESULT_LIMIT = Number(process.env.ALEXANDRIA_AUTO_RECALL_LIMIT ?? "5");
const MIN_SIMILARITY = Number(process.env.ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY ?? "0.5");

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

export default function alexandriaExtension(pi: ExtensionAPI) {
	if (!RECALL_DISABLED) {
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
				resetClient();
				ctx.ui.notify(
					`Alexandria auto-recall failed (${err instanceof Error ? err.message : String(err)}); continuing without it.`,
					"warning",
				);
				return;
			}
		});
	}

	pi.on("session_shutdown", async () => {
		await closeClient();
	});
}
