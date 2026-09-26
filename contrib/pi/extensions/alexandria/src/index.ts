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
 *   ALEXANDRIA_AUTO_RECALL_LIMIT            default: 10 (read with MIN_SIMILARITY; see docs/minilm-test-data.md)
 *   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY   default: 0.45 (only valid at LIMIT=10)
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
import { asFailure } from "./failure.js";
import { SessionDedupBuffer } from "./detectors/types.js";
import { detectCorrection } from "./detectors/correction.js";
import { detectPreference } from "./detectors/preference.js";
import { trackToolStore } from "./detectors/tool-tracker.js";
import { ErrorTracker } from "./detectors/error-tracker.js";
import { PROMPT_BUDGET_MS, prewarm, connectionsEstablished } from "./mcp-client.js";
import { recordPromptPath, startDriftProbe, outcomeLabel } from "./diag.js";
import { runExtraction } from "./extraction.js";
import { sessionArgs } from "./session-args.js";

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

function notifyStoreFailed(
	ctx: { ui: { notify(message: string, level: "warning"): void } },
	err: unknown,
): void {
	ctx.ui.notify(
		`Alexandria store_memory failed (${err instanceof Error ? err.message : String(err)})`,
		"warning",
	);
}

export default function alexandriaExtension(pi: ExtensionAPI) {
	// Session-scoped state — reset on each session
	let dedupBuffer = new SessionDedupBuffer();
	let errorTracker = new ErrorTracker();

	// ── Combined injection dispatcher (recall + reminders) ──────────────
	if (!CONFIG.recallDisabled || !CONFIG.remindersDisabled) {
		// Open the connection now rather than on the first prompt. The cold handshake
		// is the one window measured to turn a healthy server into a false
		// `REQUEST_TIMEOUT` when pi's event loop stalls, and this is what takes it off
		// the prompt path. Not awaited: extension load must not block on the server.
		prewarm();

		pi.on("before_agent_start", async (event, ctx) => {
			const query = event.prompt?.trim();
			// Diagnostics start before any await so the recorded window covers the
			// whole handler, including the git probe and any stall of pi's own loop.
			const startedAt = Date.now();
			const generationBefore = connectionsEstablished();
			const stopDriftProbe = startDriftProbe();
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

			/**
			 * Which abort source fired, read at catch time — both signals are still
			 * live until then, so sampling them earlier would classify a cancellation
			 * that had not happened yet.
			 *
			 * `ctx.signal` is pi's: the operator pressed Esc, a newer prompt superseded
			 * this one, or the session reloaded. `budget` is ours. They are not the same
			 * claim, and the distinction is the whole fix: the SDK reports an aborted
			 * request as REQUEST_TIMEOUT, so without this context an operator's Esc is
			 * indistinguishable from a dead server — it used to warn "Alexandria
			 * unreachable" *and* drop a healthy MCP session.
			 *
			 * Our budget firing while pi's signal is intact is the only combination that
			 * says the prompt path ran out of time rather than being told to stop.
			 */
			const failureContext = () => {
				const aborted = ctx.signal?.aborted === true;
				return { aborted, budgetExceeded: budget.signal.aborted && !aborted };
			};

			// Settled, not awaited: one feature failing must never suppress the
			// other's injection or its notification.
			const recallTask: Promise<FeatureOutcome> =
				!CONFIG.recallDisabled && query
					? (async () => {
							try {
								const { items, error } = await retrieveMemories(
									query,
									undefined,
									budget.signal,
								);
								return {
									block: items.length > 0 ? formatMemoriesBlock(items) : null,
									error,
								};
							} catch (err) {
								throw asFailure(err, failureContext());
							}
						})()
					: Promise.resolve({ block: null });

			const remindersTask: Promise<FeatureOutcome> = CONFIG.remindersDisabled
				? Promise.resolve({ block: null, count: 0 })
				: (async () => {
						try {
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
						} catch (err) {
							throw asFailure(err, failureContext());
						}
					})();

			const [recallRes, remindersRes] = await Promise.allSettled([
				recallTask,
				remindersTask,
			]);
			clearTimeout(timer);
			// Stopped here rather than at the record below, so the probe cannot outlive
			// the awaits it measures: everything after this point is synchronous, and a
			// throw in it would otherwise leak one interval per prompt. The window that
			// matters — the two settled tasks, where a stall would actually land — is
			// already closed.
			const driftMs = stopDriftProbe();

			// Per-feature failure isolation, the merged message, and the wording
			// for each failure mode all live in `buildInjection` — extracted so the
			// cross-product of settled outcomes is testable without a server, which
			// is the claim this dispatcher exists to make.
			const injection = buildInjection(recallRes, remindersRes);
			if (injection.resetClient) {
				// Only a failure that implicates the connection reaches here —
				// `buildInjection` has already excluded cancellations and prompt-budget
				// stalls, where the session is healthy. Dropping it is still right for
				// the rest: a cached rejection would otherwise be replayed on every
				// later prompt and delivery could never resume. `callToolWithRetry`
				// already handled the stale-session reconnect.
				resetClient();
			}
			for (const n of injection.notifications) notify(n.text, n.level);

			// Recorded last, and never awaited: `recordPromptPath` is fire-and-forget
			// and swallows its own failures, so a diagnostic cannot delay or break the
			// prompt it is measuring. `cold` is the field that makes the log worth
			// keeping — a false timeout on a cold connect is the mechanism measured in
			// the lab, and one on a warm connection would be something else entirely.
			recordPromptPath({
				t: new Date(startedAt).toISOString(),
				ms: Date.now() - startedAt,
				cold: connectionsEstablished() !== generationBefore,
				driftMs,
				recall: outcomeLabel(recallRes),
				reminders: outcomeLabel(remindersRes),
			});
			// Returned last on purpose: nothing a notification does can now discard an
			// injection the server has already consumed.
			return injection.message ? { message: injection.message } : undefined;
		});
	}

	// ── Store: Heuristic detectors ──────────────────────────────────────
	if (!CONFIG.storeDisabled) {
		// Correction + preference detection on user prompts
		pi.on("before_agent_start", async (event, ctx) => {
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
				storeMemory(detection.content, detection.tags, sessionArgs(ctx)).catch((err) =>
					notifyStoreFailed(ctx, err),
				);
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
		pi.on("agent_end", async (_event, ctx) => {
			const resolutions = errorTracker.flush();
			for (const mem of resolutions) {
				storeMemory(mem.content, mem.tags, sessionArgs(ctx)).catch((err) =>
					notifyStoreFailed(ctx, err),
				);
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
					await storeMemory(mem.content, [...mem.tags, "extracted"], sessionArgs(ctx)).catch((err) =>
						notifyStoreFailed(ctx, err),
					);
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
