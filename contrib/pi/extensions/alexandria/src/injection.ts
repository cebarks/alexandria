/**
 * The merge step of the per-prompt injection: two independently settled tasks
 * (recall and reminders) become one injected message plus the notifications to
 * show. Extracted from the `before_agent_start` handler for two reasons.
 *
 * 1. **Testability.** The handler is an inline closure over directly imported
 *    functions, which the test runner cannot reach under tsx (module mocking
 *    hangs). Per-feature failure isolation is the load-bearing claim of the merged
 *    dispatcher, so it lives here where it can be table-tested instead of being
 *    asserted by inspection.
 * 2. **Ordering safety.** Notifications are *returned*, not sent. `ctx.ui` is a
 *    lazy getter that throws once the runner is invalidated (a session switch or
 *    reload during the several seconds this waits), and a throw after
 *    `check_reminders` had already consumed its rows would discard a delivery
 *    *and* the recall block with it. Deciding the message before anyone touches
 *    the UI removes that window entirely.
 */

/** What one feature's probe produced: the block to inject (null when there is
 *  nothing to inject), and a server-reported or unparseable failure worth telling
 *  the user about — which is *not* a transport failure, since the reply arrived.
 *  `count` is the reminder path's row count, absent for recall. */
export interface FeatureOutcome {
	block: string | null;
	error?: string;
	count?: number;
}

/** Backwards-compatible name for the reminder shape. */
export type ReminderOutcome = FeatureOutcome;

/** One settled task, exactly as `Promise.allSettled` reports it. */
export type Settled<T> = PromiseSettledResult<T>;

export interface Notification {
	text: string;
	level: "info" | "warning";
}

export interface Injection {
	/** The merged message to inject, or undefined when there is nothing to say.
	 *  pi accumulates `message` per handler, so returning undefined leaves other
	 *  extensions' injections untouched. */
	message?: {
		customType: string;
		content: string;
		display: true;
	};
	/** What to show the human, in order. The caller sends these guarded. */
	notifications: Notification[];
	/** Drop the shared MCP client: at least one task did not reach a usable
	 *  server response, and a cached rejection would otherwise be replayed on
	 *  every later prompt so delivery could never resume. */
	resetClient: boolean;
}

/** Short, readable form of a rejection for a user-facing warning. */
export function reasonText(reason: unknown): string {
	return reason instanceof Error ? reason.message : String(reason);
}

/**
 * Merge the two settled tasks. Both orders of business are preserved from the
 * original inline handler: recall first, reminders second (deterministic order,
 * exactly one injected message per prompt), and a shared failure cause is said
 * once while distinct causes are both reported.
 */
export function buildInjection(
	recall: Settled<FeatureOutcome>,
	reminders: Settled<ReminderOutcome>,
): Injection {
	const notifications: Notification[] = [];
	const blocks: string[] = [];

	if (recall.status === "fulfilled" && recall.value.block) {
		blocks.push(recall.value.block);
	}
	if (reminders.status === "fulfilled" && reminders.value.block) {
		blocks.push(reminders.value.block);
		// The injected block is the agent-visible half. The human-visible half is
		// only an *announcement*: pi's notify is a no-op with no dialog UI (print
		// mode, `pi -p`), so this cannot promise the user saw it — it says what the
		// agent was given, and the agent half is the guarantee.
		notifications.push({
			text: `⏰ ${reminders.value.count ?? 0} Alexandria reminder(s) due (injected into context)`,
			level: "info",
		});
	}

	let failure: string | null = null;
	if (recall.status === "rejected") {
		const rc = reasonText(recall.reason);
		failure =
			reminders.status === "rejected"
				? reminders.reason !== undefined && rc === reasonText(reminders.reason)
					? `Alexandria unreachable (${rc}); continuing without recall or reminders.`
					: `Alexandria recall failed (${rc}) and the reminder check failed (${reasonText(reminders.reason)}); continuing without either.`
				: `Alexandria auto-recall failed (${rc}); continuing without it.`;
	} else if (reminders.status === "rejected") {
		failure = `Alexandria reminders check failed (${reasonText(reminders.reason)}); continuing without it.`;
	}

	// A rejection means this call did not reach a usable server response
	// (unreachable, timed out, or bad config) — `callToolWithRetry` already handled
	// the stale-session reconnect. Both features share one client, so either
	// rejection drops it.
	const resetClient = failure !== null;
	if (failure !== null) {
		notifications.push({ text: failure, level: "warning" });
	}

	// A server-reported failure or an unparseable payload is not a transport
	// failure: the reply came back, it just carried nothing usable. Warn (once per
	// prompt, but every prompt — a permanently broken path should stay visible) and
	// keep the client, since an error response consumes no rows.
	for (const [name, outcome] of [
		["recall", recall] as const,
		["reminders", reminders] as const,
	]) {
		if (outcome.status !== "fulfilled" || !outcome.value.error) continue;
		const tail =
			name === "reminders"
				? "nothing was consumed, retrying next prompt."
				: "continuing without it.";
		notifications.push({
			text: `Alexandria ${name === "recall" ? "auto-recall" : "reminders check"} failed (${outcome.value.error}); ${tail}`,
			level: "warning",
		});
	}

	if (blocks.length === 0) {
		return { notifications, resetClient };
	}
	return {
		message: {
			customType: "alexandria",
			content: blocks.join("\n\n"),
			display: true,
		},
		notifications,
		resetClient,
	};
}
