/**
 * Alexandria Companion Extension (v2.1)
 *
 * Recall (before_agent_start):
 *   Queries Alexandria for memories relevant to the user's prompt and injects
 *   them into context before the agent starts. Disable: ALEXANDRIA_AUTO_RECALL=off
 *
 * Storage is explicit: agents use the Alexandria memory skill and authenticated
 * store_memory tool when they decide a durable memory should be saved.
 *
 * Project context, recall, and reminders (before_agent_start):
 *   Resolves the session cwd's sanitized origin through resolve_project, or uses the
 *   explicit ALEXANDRIA_PROJECT_ID override. No origin means Personal-only, never a
 *   guessed basename. The issued ID scopes recall and reminder checks and is included
 *   in the agent-visible context. Delivery and escalation stay within the account.
 *   Accepted due rows are injected and announced; consuming a fire advances its schedule.
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
 *   ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY   default: 0.45 (calibrated for LIMIT=10)
 *   ALEXANDRIA_REMINDERS                    set to "off" to disable reminder checks
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { CONFIG, RECALL_SETTINGS } from "./config.js";
import { showRecallSettings } from "./recall-settings.js";
import {
	authenticatedServer,
	closeClient,
	connectionsEstablished,
	prewarm,
	PROMPT_BUDGET_MS,
	resetClient,
} from "./mcp-client.js";
import { retrieveMemories, formatMemoriesBlock } from "./recall.js";
import { checkReminders, formatDueBlock } from "./reminders.js";
import { getProjectContext } from "./project-context.js";
import { buildInjection, type FeatureOutcome } from "./injection.js";
import { asFailure } from "./failure.js";
import { recordPromptPath, startDriftProbe, outcomeLabel } from "./diag.js";

interface ExtensionDependencies {
	authenticatedServer: typeof authenticatedServer;
	prewarm: typeof prewarm;
	retrieveMemories: typeof retrieveMemories;
	checkReminders: typeof checkReminders;
}

export default function alexandriaExtension(
	pi: ExtensionAPI,
	dependencies: ExtensionDependencies = { authenticatedServer, prewarm, retrieveMemories, checkReminders },
) {
	pi.registerCommand("alexandria", {
		description: "Configure Alexandria auto-recall",
		handler: async (_args, ctx) => {
			if (!ctx.hasUI) return;
			await showRecallSettings(ctx.ui, RECALL_SETTINGS, dependencies.prewarm);
		},
	});

	// ── Combined injection dispatcher (recall + reminders) ──────────────
	{
		// Open the connection now rather than on the first prompt. The cold handshake
		// is the one window measured to turn a healthy server into a false
		// `REQUEST_TIMEOUT` when pi's event loop stalls, and this is what takes it off
		// the prompt path. Not awaited: extension load must not block on the server.
		if (!CONFIG.recallDisabled || !CONFIG.remindersDisabled) dependencies.prewarm();

		pi.on("before_agent_start", async (event, ctx) => {
			const recallSettings = RECALL_SETTINGS.get().effective;
			if (!recallSettings.enabled && CONFIG.remindersDisabled) return undefined;
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
			// is already bounded by its configured per-call limit but they stack — git probe,
			// handshake, call, and
			// a reconnect retry — and pi waits for this handler before the turn starts,
			// so the number a user actually experiences is the sum. Aborting also stops
			// spending a socket on a prompt that is no longer wanted.
			const budget = new AbortController();
			const timer = setTimeout(
				() => budget.abort(new Error(`Alexandria prompt budget (${PROMPT_BUDGET_MS} ms) exceeded`)),
				PROMPT_BUDGET_MS,
			);
			const abortFromPi = () => budget.abort(ctx.signal?.reason);
			if (ctx.signal?.aborted) abortFromPi();
			else ctx.signal?.addEventListener("abort", abortFromPi, { once: true });

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

			// One authenticated project context feeds both tool calls and the injection;
			// if resolution fails, neither feature is allowed to widen to an unscoped read.
			const projectContextTask = Promise.resolve().then(async () => {
				const server = dependencies.authenticatedServer();
				const project = await getProjectContext(ctx, server, budget.signal);
				return { project, call: server.callTool };
			});

			// Settled, not awaited: one feature failing must never suppress the
			// other's injection or its notification.
			const recallTask: Promise<FeatureOutcome> =
				recallSettings.enabled && query
					? (async () => {
							try {
								const { project, call } = await projectContextTask;
								const { items, error } = await dependencies.retrieveMemories(
									query,
									call,
									budget.signal,
									project.projectId
										? { ...recallSettings, projectId: project.projectId }
										: recallSettings,
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
							const { project, call } = await projectContextTask;
							const { items, error } = await dependencies.checkReminders(
								project.projectId,
								call,
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

			const [recallRes, remindersRes, projectContextRes] = await Promise.allSettled([
				recallTask,
				remindersTask,
				projectContextTask,
			]);
			clearTimeout(timer);
			ctx.signal?.removeEventListener("abort", abortFromPi);
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
			const injection = buildInjection(
				recallRes,
				remindersRes,
				projectContextRes.status === "fulfilled" ? projectContextRes.value.project : undefined,
			);
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

	// ── Session shutdown ────────────────────────────────────────────────
	pi.on("session_shutdown", async () => {
		await closeClient();
	});
}
