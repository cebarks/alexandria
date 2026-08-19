/**
 * Tool dedup tracker — watches tool_result events for agent-initiated
 * store_memory/update_memory calls and records their content in the dedup buffer.
 *
 * MCP tool names are server-prefixed (e.g., "alexandria_store_memory").
 * We match by suffix to be resilient to prefix changes.
 */

import type { SessionDedupBuffer } from "./types.js";

const STORE_TOOL_SUFFIXES = ["store_memory", "update_memory"];

interface ToolResultLike {
	toolName: string;
	input: Record<string, unknown>;
	isError: boolean;
}

function isStoreToolCall(toolName: string): boolean {
	return STORE_TOOL_SUFFIXES.some((suffix) => toolName.endsWith(suffix));
}

/**
 * If this tool_result is a successful store_memory or update_memory call,
 * record the content in the dedup buffer. Returns true if recorded.
 */
export async function trackToolStore(event: ToolResultLike, buffer: SessionDedupBuffer): Promise<boolean> {
	if (!isStoreToolCall(event.toolName)) return false;
	if (event.isError) return false;

	const content = event.input.content;
	if (typeof content !== "string" || content.length === 0) return false;

	await buffer.addToolStore(content);
	return true;
}
