/**
 * Tool dedup tracker — watches tool_result events for agent-initiated
 * store_memory/update_memory calls and records their content in the dedup buffer.
 *
 * Two naming schemes are in use across the fleet: Pi's built-in MCP names tools
 * `mcp__<server>__<tool>` (`mcp__alexandria__store_memory`), and pi-mcp-adapter
 * 2.x — still running on mars — used `<server>_<tool>` (`alexandria_store_memory`).
 * Both reduce to the bare tool name below, which is then matched exactly.
 *
 * Matching exactly rather than by suffix matters: a suffix test also accepts
 * another server's `mcp__agentmemory__auto_store_memory`, whose content was never
 * stored in Alexandria, so the buffer would suppress a real extraction.
 *
 * What is deliberately NOT handled is pi-mcp-adapter 3.x, which published neither
 * name: it exposed a `mcp` gateway and `mcp__<server>` proxies whose real tool sat
 * in `input.tool` with arguments nested under `input.args`. This tracker matched
 * nothing at all under it, which is how agent-initiated stores went unrecorded for
 * a year. 3.x is uninstalled; re-adding that shape needs a test fixture for it.
 */

import type { SessionDedupBuffer } from "./types.js";

const STORE_TOOLS = new Set(["store_memory", "update_memory"]);

interface ToolResultLike {
	toolName: string;
	input: Record<string, unknown>;
	isError: boolean;
}

/**
 * The tool name as its MCP server offered it, with the client's server prefix
 * removed: Pi separates with `__`, adapter 2.x with a single `_`.
 */
function bareToolName(toolName: string): string {
	const sep = toolName.lastIndexOf("__");
	if (sep >= 0) return toolName.slice(sep + 2);
	return toolName.replace(/^[A-Za-z0-9-]+_/, "");
}

function isStoreToolCall(toolName: string): boolean {
	return STORE_TOOLS.has(toolName) || STORE_TOOLS.has(bareToolName(toolName));
}

/**
 * If this tool_result is a successful store_memory or update_memory call,
 * record the content in the dedup buffer. Returns true if recorded.
 */
export function trackToolStore(
	event: ToolResultLike,
	buffer: SessionDedupBuffer,
): boolean {
	if (!isStoreToolCall(event.toolName)) return false;
	if (event.isError) return false;

	const content = event.input.content;
	if (typeof content !== "string" || content.length === 0) return false;

	buffer.addToolStore(content);
	return true;
}
