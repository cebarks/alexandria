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
import { buildInjection, type FeatureOutcome } from "./injection.js";
import { SessionDedupBuffer } from "./detectors/types.js";
import { detectCorrection } from "./detectors/correction.js";
import { detectPreference } from "./detectors/preference.js";
import { trackToolStore } from "./detectors/tool-tracker.js";
import { ErrorTracker } from "./detectors/error-tracker.js";
import { PROMPT_BUDGET_MS } from "./mcp-client.js";
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
			// Resolved before any await, and every use guarded: `ctx.ui` is a lazy
			// getter that throws once the runner is invalidated (a session switch or
			// reload during the up-to-12 s this handler waits), and an uncaught throw
			// here would reject the handler — dropping both injections *after*
			// check_reminders had already consumed its rows.
			const ui = ctx.ui;
			const notify = (message: string, level: "info" | "warning") => {
				try {
					ui.notify(message, level);
				} catch {
					/* the human half is best-effort; the agent half already succeeded */
				}
			};

			// One deadline for the whole prompt path, not one per attempt. Each call
			// is already bounded (5 s) but they stack — git probe, handshake, call, and
			// a reconnect retry — and pi waits for this handler before the turn starts,
			// so the number a user actually experiences is the sum. Aborting also stops
			// spending a socket on a prompt that is no longer wanted.
			const budget = new AbortController();
			const timer = setTimeout(
				() => budget.abort(new Error(`Alexandria prompt budget (${PROMPT_BUDGET_MS} ms) exceeded`)),
				PROMPT_BUDGET_MS,
			);
			ctx.signal?.addEventListener("abort", () => budget.abort(ctx.signal?.reason), {
				once: true,
			});

			// Settled, not awaited: one feature failing must never suppress the
			// other's injection or its notification.
			const recallTask: Promise<FeatureOutcome> =
				!CONFIG.recallDisabled && query
					? (async () => {
							const { items, error } = await retrieveMemories(
								query,
								undefined,
								budget.signal,
							);
							return {
								block: items.length > 0 ? formatMemoriesBlock(items) : null,
								error,
							};
						})()
					: Promise.resolve({ block: null });

			const remindersTask: Promise<FeatureOutcome> = CONFIG.remindersDisabled
				? Promise.resolve({ block: null, count: 0 })
				: (async () => {
						const project = await getProjectHint(ctx.cwd);
						const { items, error } = await checkReminders(
							project,
							undefined,
							budget.signal,
						);
						return {
							block: items.length > 0 ? formatDueBlock(items) : null,
							count: items.length,
							error,
						};
					})();

			const [recallRes, remindersRes] = await Promise.allSettled([
				recallTask,
				remindersTask,
			]);
			clearTimeout(timer);

			// Per-feature failure isolation, the merged message, and the wording
			// for each failure mode all live in `buildInjection` — extracted so the
			// cross-product of settled outcomes is testable without a server, which
			// is the claim this dispatcher exists to make.
			const injection = buildInjection(recallRes, remindersRes);
			if (injection.resetClient) {
				// A rejection means this call did not reach a usable server response
				// (unreachable, timed out, or bad config) — `callToolWithRetry` already
				// handled the stale-session reconnect. Both features share one client,
				// so either rejection drops it; a cached rejection would otherwise be
				// replayed on every later prompt, and delivery would never resume.
				resetClient();
			}
			for (const n of injection.notifications) notify(n.text, n.level);
			// Returned last on purpose: nothing a notification does can now discard an
			// injection the server has already consumed.
			return injection.message ? { message: injection.message } : undefined;
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
