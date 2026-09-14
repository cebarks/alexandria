/**
 * Reminders feature: check Alexandria for due reminders on each prompt.
 *
 * The server runs no timer: a reminder becomes visible only when someone calls
 * `check_reminders`, which also consumes what it returns (recurring schedules
 * advance to their next fire, coalescing skipped occurrences into
 * `missed_occurrences`). That makes this call the delivery path for the pi
 * companion, so it runs once per prompt and its output goes straight into the
 * injected context.
 *
 * Fail-open everywhere — an unreachable server never blocks the turn.
 */

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { basename } from "node:path";
import {
	callToolWithRetry,
	extractTextContent,
	PROMPT_CALL_TIMEOUT_MS,
} from "./mcp-client.js";
import { CONFIG } from "./config.js";

const execFileAsync = promisify(execFile);

export interface DueReminder {
	id: string;
	message: string;
	/** `"global"` or `"project:<name>"`. */
	target?: string;
	escalated?: boolean;
	recurring?: boolean;
	missed_occurrences?: number;
	/** RFC 3339, Z-suffixed. */
	due_at?: string;
	/** Human-readable schedule, e.g. "daily at 09:00". */
	schedule?: string;
	note?: string | null;
	provenance?: { project?: string | null; session_id?: string | null };
	/**
	 * RFC 3339, Z-suffixed. For a recurring row, the fire it advanced to; absent
	 * (null) when the reminder is finished — one-shot, or a schedule with no
	 * future fire left.
	 */
	next_due_at?: string | null;
}

/** Memoized result of the git probe: `null` means "not probed yet". The cwd of
 *  the extension process cannot change within a pi session, and this runs in
 *  front of every per-prompt check, so a settled outcome is measured once and
 *  reused. Only *deterministic* outcomes are memoized — a transient failure is
 *  retried on the next prompt. */
let projectHint: string | undefined | null = null;

/** Project hint for delivery targeting. Env override wins (direnv-friendly,
 *  also fixes git-worktree dirs whose basename differs from the project).
 *  Caching a failure would be permanent for the life of the process, and a
 *  session pinned to no hint holds every project-targeted reminder until
 *  escalation, so a killed probe is left uncached. */
export async function getProjectHint(): Promise<string | undefined> {
	if (CONFIG.remindersProject) return CONFIG.remindersProject;
	if (projectHint !== null) return projectHint;

	let hint: string | undefined;
	try {
		const { stdout } = await execFileAsync(
			"git",
			["rev-parse", "--show-toplevel"],
			{ timeout: 2000 },
		);
		const root = stdout.trim();
		hint = root ? basename(root) : undefined;
	} catch (err) {
		// A missing git or a non-repo cwd is deterministic: cache it. A killed child
		// (the 2s budget) is not — leave it un-cached so the next prompt retries.
		if ((err as { killed?: boolean }).killed) return undefined;
		hint = undefined; // not a repo, git missing — global reminders still work
	}
	projectHint = hint;
	return hint;
}

/** Drop the memoized probe. Test hook: the fallback path is otherwise
 *  unobservable once one successful probe has settled the module state. */
export function __resetProjectHint(): void {
	projectHint = null;
}

/** The outcome of one delivery check: what to inject, plus a server-reported or
 *  unparseable failure worth telling the user about. `error` never means rows
 *  were consumed — the next prompt retries. */
export interface ReminderCheck {
	items: DueReminder[];
	/** Set when the response carried no usable `delivered` list: either a
	 *  `{"status":"error",...}` reply from the server or unparseable JSON. */
	error?: string;
}

export async function checkReminders(project?: string): Promise<ReminderCheck> {
	const args: Record<string, unknown> = project ? { project } : {};
	// Bounded like recall, because this is the delivery path and it awaits before
	// the turn. One cost is specific to here: a check that gives up may already
	// have consumed its rows server-side, so that one fire is lost.
	const result = await callToolWithRetry(
		"check_reminders",
		args,
		PROMPT_CALL_TIMEOUT_MS,
	);
	const text = extractTextContent(result.content);
	if (!text) return { items: [] };
	let parsed: { delivered?: unknown; status?: unknown; message?: unknown };
	try {
		parsed = JSON.parse(text) as typeof parsed;
	} catch {
		// Unparseable payload: fail open, but name it — silently reporting
		// "nothing due" would hide a permanently broken delivery path.
		return { items: [], error: "malformed check_reminders response" };
	}
	// A successful check always carries a `delivered` array, so anything else is
	// the server's {"status":"error",...} shape (or a payload we cannot read).
	// Either way nothing was consumed, so this warns once and retries next turn.
	if (!Array.isArray(parsed.delivered)) {
		const detail =
			typeof parsed.message === "string" && parsed.message.trim() !== ""
				? parsed.message
				: typeof parsed.status === "string"
					? `status ${parsed.status}`
					: "no delivered list in response";
		return { items: [], error: detail };
	}
	// The payload came from JSON, so its shape is not the type's promise: keep
	// only the objects formatDueBlock can actually read.
	return {
		items: parsed.delivered.filter(
			(item): item is DueReminder => !!item && typeof item === "object",
		),
	};
}

/** Collapse newlines so one reminder stays one line in the injected block. */
const oneLine = (s: string) => s.replace(/\s*\n\s*/g, " ");

export function formatDueBlock(items: DueReminder[]): string {
	const lines = items.map((r) => {
		// The text arrives as unvalidated JSON, and this block is one bullet per
		// reminder: never print `undefined`, and never let a newline read as a
		// continuation of the previous item — so every interpolated field goes
		// through oneLine, not just the message.
		const message =
			typeof r.message === "string" && r.message.trim() !== ""
				? oneLine(r.message)
				: "(reminder with no text)";
		const bits: string[] = [`- ${message}`];
		if (r.target && r.target !== "global")
			bits.push(`[target ${oneLine(r.target)}]`);
		if (r.escalated) bits.push("[OVERDUE — escalated from project targeting]");
		if (r.missed_occurrences && r.missed_occurrences > 0)
			bits.push(`(missed ${r.missed_occurrences} earlier occurrence(s))`);
		if (r.due_at) bits.push(`(due ${oneLine(r.due_at)})`);
		if (r.schedule) bits.push(`(${oneLine(r.schedule)})`);
		if (r.recurring)
			bits.push(
				r.next_due_at
					? `(⟲ recurring — next fire ${oneLine(r.next_due_at)})`
					: "(⟲ recurring — final fire, nothing scheduled after this)",
			);
		if (r.note) bits.push(`note: ${oneLine(r.note)}`);
		const prov = r.provenance;
		if (prov?.project || prov?.session_id) {
			const session = prov.session_id
				? `, session ${oneLine(prov.session_id)}`
				: "";
			bits.push(
				`[set in ${oneLine(prov.project ?? "unknown project")}${session}]`,
			);
		}
		return bits.join(" ");
	});
	return [
		"⏰ Due reminders from Alexandria (each of these is consumed now and will not be repeated unless it is marked recurring — act on it or tell the user):",
		...lines,
	].join("\n");
}
