/**
 * Auto-recall: queries Alexandria for memories relevant to the user's prompt.
 */

import { getClient, extractTextContent } from "./mcp-client.js";

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

export async function retrieveMemories(query: string): Promise<RetrievedMemory[]> {
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

export function formatMemoriesBlock(memories: RetrievedMemory[]): string {
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
