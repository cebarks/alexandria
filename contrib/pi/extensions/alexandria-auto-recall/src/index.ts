/**
 * Alexandria Auto-Recall & Auto-Store Extension (v2.0)
 *
 * Recall (before_agent_start):
 *   Queries Alexandria for memories relevant to the user's prompt and injects
 *   them into context before the agent starts. Disable: ALEXANDRIA_AUTO_RECALL=off
 *
 * Store — three layers:
 *   1. Skill (existing, separate SKILL.md): agent decides during conversation
 *   2. Heuristic detectors (this extension):
 *      - Correction detector: "no, use X" → store_memory
 *      - Preference detector: "always do X" → store_memory
 *      - Error resolution tracker: error→success pairs → store_memory
 *      - Tool dedup tracker: records agent-initiated stores for extraction dedup
 *   3. LLM extraction (this extension, session_shutdown):
 *      Serializes conversation, asks a cheap model to extract remaining durable facts
 *
 *   Disable all store behavior: ALEXANDRIA_AUTO_STORE=off
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
import { CONFIG } from "./config.js";
import { resetClient, closeClient, storeMemory } from "./mcp-client.js";
import { retrieveMemories, formatMemoriesBlock } from "./recall.js";
import { SessionDedupBuffer } from "./detectors/types.js";
import { detectCorrection } from "./detectors/correction.js";
import { detectPreference } from "./detectors/preference.js";
import { trackToolStore } from "./detectors/tool-tracker.js";
import { ErrorTracker } from "./detectors/error-tracker.js";
import { runExtraction } from "./extraction.js";

export default function alexandriaExtension(pi: ExtensionAPI) {
	// Session-scoped state — reset on each session
	let dedupBuffer = new SessionDedupBuffer();
	let errorTracker = new ErrorTracker();

	// ── Recall (existing behavior) ──────────────────────────────────────
	if (!CONFIG.recallDisabled) {
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

	// ── Store: Heuristic detectors ──────────────────────────────────────
	if (!CONFIG.storeDisabled) {
		// Correction + preference detection on user prompts
		pi.on("before_agent_start", async (event) => {
			const prompt = event.prompt?.trim();
			if (!prompt) return;

			const detections = [
				detectCorrection(prompt, dedupBuffer),
				detectPreference(prompt, dedupBuffer),
			].filter((d): d is NonNullable<typeof d> => d !== null);

			// Fire-and-forget stores — don't block the agent turn
			for (const detection of detections) {
				storeMemory(detection.content, detection.tags).catch(() => {
					resetClient();
				});
			}
		});

		// Tool dedup tracker — watch for agent-initiated store_memory calls
		pi.on("tool_result", async (event) => {
			// ToolResultEvent variants all extend ToolResultEventBase which has
			// toolName, input, and isError. CustomToolResultEvent covers MCP tools.
			try {
				trackToolStore(
					{
						toolName: "toolName" in event ? (event.toolName as string) : "",
						input: "input" in event ? (event.input as Record<string, unknown>) : {},
						isError: event.isError,
					},
					dedupBuffer,
				);
			} catch { /* never fail on tracking */ }
		});

		// Error resolution tracker — accumulate errors and successes
		pi.on("tool_execution_end", async (event) => {
			if (event.isError) {
				errorTracker.recordError(event.toolName, event.result);
			} else {
				errorTracker.recordSuccess(event.toolName, event.result);
			}
		});

		// Flush error resolutions at agent_end
		pi.on("agent_end", async () => {
			const resolutions = errorTracker.flush();
			for (const mem of resolutions) {
				storeMemory(mem.content, mem.tags).catch(() => {});
			}
		});
	}

	// ── Session shutdown ────────────────────────────────────────────────
	pi.on("session_shutdown", async (event, ctx) => {
		// LLM extraction — skip on reload (no meaningful conversation boundary)
		if (!CONFIG.storeDisabled && event.reason !== "reload") {
			try {
				const extracted = await runExtraction(
					ctx as Parameters<typeof runExtraction>[0],
					dedupBuffer,
				);
				for (const mem of extracted) {
					await storeMemory(mem.content, [...mem.tags, "extracted"]).catch(() => {});
				}
			} catch (err) {
				ctx.ui.notify(
					`Alexandria extraction failed (${err instanceof Error ? err.message : String(err)}); skipping.`,
					"warning",
				);
			}
		}

		// Reset session state
		dedupBuffer = new SessionDedupBuffer();
		errorTracker = new ErrorTracker();

		// Close MCP client
		await closeClient();
	});

	// Reset state on session_start (handles /new, /resume, /fork)
	pi.on("session_start", async () => {
		dedupBuffer = new SessionDedupBuffer();
		errorTracker = new ErrorTracker();
	});
}
