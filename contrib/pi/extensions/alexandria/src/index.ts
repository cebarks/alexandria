/**
 * Alexandria Companion Extension (v2.1)
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
 * Reminders (before_agent_start, alongside recall):
 *   Calls check_reminders once per prompt with a project hint, so reminders the
 *   server has no timer for actually reach the user. Due ones are injected into
 *   context (agent-visible) and surfaced as a notification (human-visible);
 *   delivery is once-only, so consuming it is what advances recurring schedules.
 *   Disable: ALEXANDRIA_REMINDERS=off
 *
 * Recall and reminders share a single before_agent_start handler: both blocks
 * are resolved concurrently and merged into one injected message, for a
 * deterministic order and exactly one injected message per prompt.
 *
 * Config (env vars, all optional):
 *   ALEXANDRIA_URL                          default: http://127.0.0.1:3000/mcp
 *   ALEXANDRIA_AUTO_RECALL                  set to "off" to disable recall
 *   ALEXANDRIA_AUTO_RECALL_LIMIT            default: 5
 *   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY   default: 0.58 (too high for MiniLM; 0.35 recommended, see config.ts)
 *   ALEXANDRIA_AUTO_STORE                   set to "off" to disable all store behavior
 *   ALEXANDRIA_EXTRACT_MODEL                default: vertex/claude-haiku-4-5
 *   ALEXANDRIA_EXTRACT_TIMEOUT_MS           default: 5000
 *   ALEXANDRIA_REMINDERS                    set to "off" to disable reminder checks
 *   ALEXANDRIA_REMINDERS_PROJECT            project hint for reminder targeting
 *                                           (default: git repo directory name)
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { CONFIG } from "./config.js";
import {
	resetClient,
	closeClient,
	storeMemory,
	extractTextContent,
} from "./mcp-client.js";
import { retrieveMemories, formatMemoriesBlock } from "./recall.js";
import { getProjectHint, checkReminders, formatDueBlock } from "./reminders.js";
import { SessionDedupBuffer } from "./detectors/types.js";
import { detectCorrection } from "./detectors/correction.js";
import { detectPreference } from "./detectors/preference.js";
import { trackToolStore } from "./detectors/tool-tracker.js";
import { ErrorTracker } from "./detectors/error-tracker.js";
import { runExtraction } from "./extraction.js";

/** Short, readable form of a rejection for a user-facing warning. */
function reasonText(reason: unknown): string {
	return reason instanceof Error ? reason.message : String(reason);
}

/**
 * Extract readable text from a tool_execution_end result.
 * Handles MCP content blocks ({content: [{type: "text", text: "..."}]}),
 * plain strings, and objects with a text/message/error field.
 */
function extractResultText(result: unknown): string | null {
	if (typeof result === "string") return result;
	if (!result || typeof result !== "object") return null;

	const r = result as Record<string, unknown>;

	// MCP content block structure
	const fromContent = extractTextContent(r.content);
	if (fromContent) return fromContent;

	// Direct text/message/error fields
	for (const key of ["text", "message", "error"] as const) {
		if (typeof r[key] === "string") return r[key] as string;
	}

	return null;
}

export default function alexandriaExtension(pi: ExtensionAPI) {
	// Session-scoped state — reset on each session
	let dedupBuffer = new SessionDedupBuffer();
	let errorTracker = new ErrorTracker();

	// ── Combined injection dispatcher (recall + reminders) ──────────────
	if (!CONFIG.recallDisabled || !CONFIG.remindersDisabled) {
		pi.on("before_agent_start", async (event, ctx) => {
			const query = event.prompt?.trim();

			const recallTask: Promise<string | null> =
				!CONFIG.recallDisabled && query
					? (async () => {
							const memories = await retrieveMemories(query);
							return memories.length > 0 ? formatMemoriesBlock(memories) : null;
						})()
					: Promise.resolve(null);

			const remindersTask: Promise<{
				block: string | null;
				count: number;
				error?: string;
			}> = CONFIG.remindersDisabled
				? Promise.resolve({ block: null, count: 0 })
				: (async () => {
						const project = await getProjectHint();
						const { items, error } = await checkReminders(project);
						return {
							block: items.length > 0 ? formatDueBlock(items) : null,
							count: items.length,
							error,
						};
					})();

			// Per-feature failure isolation: one failing never suppresses the other
			const [recallRes, remindersRes] = await Promise.allSettled([
				recallTask,
				remindersTask,
			]);

			const blocks: string[] = [];
			if (recallRes.status === "fulfilled" && recallRes.value) {
				blocks.push(recallRes.value);
			}
			if (remindersRes.status === "fulfilled" && remindersRes.value.block) {
				blocks.push(remindersRes.value.block);
				// The injected block is the agent-visible half; this is the
				// human-visible one, so a delivery is not silently eaten.
				ctx.ui.notify(
					`⏰ ${remindersRes.value.count} Alexandria reminder(s) due`,
					"info",
				);
			}

			// A rejection means this call did not reach a usable server response
			// (unreachable, timed out, or bad config) — callToolWithRetry already
			// handled the stale-session reconnect. Both features share one client,
			// so either rejection drops it; a cached rejection would otherwise be
			// replayed on every later prompt, and delivery would never resume.
			let failure: string | null = null;
			if (recallRes.status === "rejected") {
				// A shared cause (one client, one outage) is said once; distinct causes
				// are both reported, or the single diagnostic names the wrong subsystem.
				const rc = reasonText(recallRes.reason);
				failure =
					remindersRes.status === "rejected"
						? remindersRes.reason !== undefined &&
							rc === reasonText(remindersRes.reason)
							? `Alexandria unreachable (${rc}); continuing without recall or reminders.`
							: `Alexandria recall failed (${rc}) and the reminder check failed (${reasonText(remindersRes.reason)}); continuing without either.`
						: `Alexandria auto-recall failed (${rc}); continuing without it.`;
			} else if (remindersRes.status === "rejected") {
				failure = `Alexandria reminders check failed (${reasonText(remindersRes.reason)}); continuing without it.`;
			}
			if (failure !== null) {
				resetClient();
				ctx.ui.notify(failure, "warning");
			}

			// A server-reported failure or an unparseable payload is not a transport
			// failure: the reply came back, it just carried nothing usable. Warn once
			// so a permanently broken delivery path is visible, but keep the client —
			// and retry next prompt, since an error response consumes no rows.
			if (remindersRes.status === "fulfilled" && remindersRes.value.error) {
				ctx.ui.notify(
					`Alexandria reminders check failed (${remindersRes.value.error}); nothing was consumed, retrying next prompt.`,
					"warning",
				);
			}

			if (blocks.length === 0) return;
			return {
				message: {
					customType: "alexandria",
					content: blocks.join("\n\n"),
					display: true,
				},
			};
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
				// storeMemory uses callToolWithRetry internally, so stale
				// sessions are recovered automatically.
				storeMemory(detection.content, detection.tags).catch(() => {});
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
			} catch {
				/* never fail on tracking */
			}
		});

		// Error resolution tracker — accumulate errors and successes
		pi.on("tool_execution_end", async (event) => {
			const text = extractResultText(event.result);
			if (!text) return;

			if (event.isError) {
				errorTracker.recordError(event.toolName, text);
			} else {
				errorTracker.recordSuccess(event.toolName, text);
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
