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

import { failureOf, type ClassifiedFailure, type FailureKind } from "./failure.js";

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

/**
 * Subject and verb for one feature's failure, by kind.
 *
 * The kind decides the wording because the old single phrasing — "failed" — told
 * the operator the server was at fault in every case, including when they had
 * pressed Esc or when pi's own loop had stalled. Only a transport failure is a
 * claim about the server, so only that one keeps the old verb.
 */
function featurePhrase(feature: "recall" | "reminders", kind: FailureKind): string {
	const what = feature === "recall" ? "auto-recall" : "reminders check";
	switch (kind) {
		case "stalled":
			return `${what} was cut short`;
		case "worker":
			return `memory client failed during ${what}`;
		default:
			return `${what} failed`;
	}
}

/**
 * Collapsed wording for when both features failed the same way with the same
 * cause — one shared cause is said once, as before.
 */
function collapsedPhrase(kind: FailureKind): string {
	switch (kind) {
		case "stalled":
			return "the Alexandria prompt path was cut short";
		case "worker":
			return "the Alexandria memory client failed";
		default:
			return "Alexandria unreachable";
	}
}

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
	/** Drop the shared MCP client: at least one task failed in a way that
	 *  implicates the connection. A cancellation or a prompt-budget stall does not
	 *  qualify — the session is fine and reconnecting would fix nothing. */
	resetClient: boolean;
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

	// A cancellation is not a failure worth reporting: the user interrupted, or pi
	// superseded the prompt, and the SDK labels that REQUEST_TIMEOUT. Warning about
	// it would blame the server for the operator's own Esc. Filtering it here rather
	// than at the throw site keeps `buildInjection` the single place that decides
	// what the human is told.
	const recallFailure =
		recall.status === "rejected" ? failureOf(recall.reason) : null;
	const remindersFailure =
		reminders.status === "rejected" ? failureOf(reminders.reason) : null;
	const rc = recallFailure?.kind === "cancelled" ? null : recallFailure;
	const rm = remindersFailure?.kind === "cancelled" ? null : remindersFailure;

	let failure: string | null = null;
	if (rc && rm) {
		failure =
			rc.kind === rm.kind && rc.cause === rm.cause
				? `${collapsedPhrase(rc.kind)} (${rc.cause}); continuing without recall or reminders.`
				: `Alexandria ${featurePhrase("recall", rc.kind)} (${rc.cause}) and the ${featurePhrase("reminders", rm.kind)} (${rm.cause}); continuing without either.`;
	} else if (rc) {
		failure = `Alexandria ${featurePhrase("recall", rc.kind)} (${rc.cause}); continuing without it.`;
	} else if (rm) {
		failure = `Alexandria ${featurePhrase("reminders", rm.kind)} (${rm.cause}); continuing without it.`;
	}

	// Only a failure that implicates the connection drops it. `cancelled` and
	// `stalled` leave it alone: the session is healthy, and resetting it would cost
	// a fresh handshake on the next prompt while fixing nothing. This used to be
	// `failure !== null`, which is how an Esc came to tear down the client.
	const resetClient =
		(rc?.resetConnection ?? false) || (rm?.resetConnection ?? false);
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
