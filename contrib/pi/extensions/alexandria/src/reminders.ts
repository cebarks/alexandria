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
  type CallTool,
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
  /**
   * `[reminders].timezone`, the zone `schedule` is written in. `due_at` and
   * `next_due_at` are UTC instants, so without this a client cannot tell the
   * user when a fire actually is.
   */
  timezone?: string;
  /** `due_at` rendered in `timezone` (`2026-09-16 09:00 +02:00`). */
  due_at_local?: string;
  /** `next_due_at` rendered in `timezone`, when there is a next fire. */
  next_due_at_local?: string | null;
  /** True when `missed_occurrences` hit the engine's iteration cap: the real
   *  number of missed fires is larger than what is shown. */
  missed_occurrences_saturated?: boolean;
}

/** Memoized git probes, keyed by the directory they ran in. `null` means "not
 *  probed yet" for that directory. Keyed rather than single because the hint is
 *  asked per *session* directory (`ctx.cwd`), and one pi process can serve
 *  sessions from several projects across `/resume`, `/new` and forks — a single
 *  slot would keep answering for the first one forever. Only *deterministic*
 *  outcomes are memoized; a transient failure is retried on the next prompt. */
const projectHints = new Map<string, string | null>();

/** Project hint for delivery targeting. Env override wins (direnv-friendly,
 *  also fixes git-worktree dirs whose basename differs from the project).
 *  `cwd` must be the **session's** directory (`ctx.cwd`), not `process.cwd()`: a
 *  resumed session can live in a different project than the one pi was launched
 *  from, and the server matches the target byte-exactly — so the wrong hint
 *  holds that project's reminders until they arrive late and escalated. */
export async function getProjectHint(
  cwd?: string,
): Promise<string | undefined> {
  if (CONFIG.remindersProject) return CONFIG.remindersProject;
  const dir = cwd ?? process.cwd();
  const cached = projectHints.get(dir);
  if (cached !== undefined) return cached ?? undefined;

  let hint: string | undefined;
  try {
    const { stdout } = await execFileAsync(
      "git",
      ["rev-parse", "--show-toplevel"],
      { cwd: dir, timeout: 2000 },
    );
    const root = stdout.trim();
    hint = root ? basename(root) : undefined;
  } catch (err) {
    // A missing git or a non-repo directory is deterministic: cache it. A killed
    // child (the 2s budget) is not — leave it un-cached so the next prompt
    // retries, because a session pinned to no hint holds every project-targeted
    // reminder until escalation.
    if ((err as { killed?: boolean }).killed) return undefined;
    hint = undefined; // not a repo, git missing — global reminders still work
  }
  projectHints.set(dir, hint ?? null);
  return hint;
}

/** Drop the memoized probes. Test hook: the fallback path is otherwise
 *  unobservable once one successful probe has settled the module state. */
export function __resetProjectHint(): void {
  projectHints.clear();
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

export async function checkReminders(
  project?: string,
  call: CallTool = callToolWithRetry,
  signal?: AbortSignal,
): Promise<ReminderCheck> {
  const args: Record<string, unknown> = project ? { project } : {};
  // Bounded like recall, because this is the delivery path and it awaits before
  // the turn. One cost is specific to here: a check that gives up may already
  // have consumed its rows server-side, so that one fire is lost.
  const result = await call(
    "check_reminders",
    args,
    PROMPT_CALL_TIMEOUT_MS,
    signal,
  );
  if (result.isError === true) {
    return {
      items: [],
      error:
        extractTextContent(result.content) ??
        "check_reminders reported an error",
    };
  }
  const text = extractTextContent(result.content);
  // A response with no text block is not "nothing due": it is a delivery path
  // that produced nothing readable, and the rows may already be consumed
  // server-side. Saying so is the whole point of the `error` channel.
  if (!text) {
    return { items: [], error: "no text content in check_reminders response" };
  }
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
  // The payload came from JSON, so its shape is not the type's promise. Validate
  // field by field rather than trusting object-ness: `formatDueBlock` renders
  // every one of them through `oneLine`, and a single non-string value there
  // throws mid-render — which the dispatcher would read as a transport failure,
  // reset a healthy client, and lose a delivery the server has already consumed.
  return {
    items: parsed.delivered
      .map(toDueReminder)
      .filter((r): r is DueReminder => r !== null),
  };
}

/** Coerce one delivered JSON object into a renderable row, or drop it. Only a
 *  row that is not an object at all, or carries no id *and* no message, is
 *  discarded: a wrong-typed optional field costs that field's marker, never the
 *  reminder itself. */
function toDueReminder(item: unknown): DueReminder | null {
  if (!item || typeof item !== "object") return null;
  const raw = item as Record<string, unknown>;
  const id = str(raw.id);
  const message = str(raw.message);
  if (!id && !message) return null;
  const prov = raw.provenance;
  const out: DueReminder = {
    id: id ?? "",
    message: message ?? "",
  };
  for (const key of [
    "target",
    "schedule",
    "note",
    "due_at",
    "next_due_at",
    "due_at_local",
    "next_due_at_local",
    "timezone",
  ] as const) {
    const v = str(raw[key]);
    if (v !== undefined) (out as unknown as Record<string, unknown>)[key] = v;
  }
  if (typeof raw.escalated === "boolean") out.escalated = raw.escalated;
  if (typeof raw.recurring === "boolean") out.recurring = raw.recurring;
  if (typeof raw.missed_occurrences_saturated === "boolean") {
    out.missed_occurrences_saturated = raw.missed_occurrences_saturated;
  }
  const missed = num(raw.missed_occurrences);
  if (missed !== undefined) out.missed_occurrences = missed;
  if (prov && typeof prov === "object") {
    const p = prov as Record<string, unknown>;
    const pp = str(p.project);
    const ps = str(p.session_id);
    if (pp || ps) out.provenance = { project: pp, session_id: ps };
  }
  return out;
}

/** A string field, or undefined for anything that is not a usable string. */
const str = (v: unknown): string | undefined =>
  typeof v === "string" && v.trim() !== "" ? v : undefined;

/** A finite number field, or undefined. `NaN`/`Infinity` in JSON would otherwise
 *  render as literally that into the agent's context. */
const num = (v: unknown): number | undefined =>
  typeof v === "number" && Number.isFinite(v) ? v : undefined;

/** Collapse newlines so one reminder stays one line in the injected block. Takes
 *  `unknown` and stringifies defensively: this runs over every field of a row
 *  that came from JSON, and a throw here is a lost delivery. */
const oneLine = (s: unknown) =>
  String(s)
    .trim()
    .replace(/\s*\n\s*/g, " ");

/** How many due rows are rendered into one prompt. The rows are consumed as they
 *  are injected, so there is no second chance to trim them, and "200 reminders
 *  piled up" should degrade to a pointer at `list_reminders` rather than a 200
 *  bullet block. */
export const MAX_RENDERED_REMINDERS = 10;

/** A fire time, spelled the way the reader can act on: local with the zone when
 *  the server supplied it, otherwise the UTC instant labelled as such. Rendering a
 *  bare `2026-09-18T14:00:00Z` next to a `schedule` written in wall clock is what
 *  leaves an agent unable to say *when* something is due. */
function when(local: string | undefined, instant: string | undefined): string {
  return local ?? (instant === undefined ? "" : `${oneLine(instant)} UTC`);
}

export function formatDueBlock(items: DueReminder[]): string {
  const shown = items.slice(0, MAX_RENDERED_REMINDERS);
  const lines = shown.map((r) => {
    // Fields were validated by `toDueReminder`, but every interpolated value
    // still goes through oneLine: this block is one bullet per reminder, and a
    // stray newline would read as a continuation of the previous item.
    // Whitespace-only text collapses to nothing, so the placeholder is chosen
    // on the *rendered* value rather than the raw one.
    const text = r.message ? oneLine(r.message) : "";
    const bits: string[] = [`- ${text || "(reminder with no text)"}`];
    // The id belongs here: `cancel_reminder` needs one, and without it an agent
    // told to "stop that one" has to make a second round trip to find it.
    if (r.id) bits.push(`[id ${oneLine(r.id)}]`);
    if (r.target && r.target !== "global")
      bits.push(`[target ${oneLine(r.target)}]`);
    if (r.escalated) bits.push("[OVERDUE — escalated from project targeting]");
    if (r.missed_occurrences && r.missed_occurrences > 0)
      bits.push(
        r.missed_occurrences_saturated
          ? `(missed ${r.missed_occurrences}+ earlier occurrence(s), count capped)`
          : `(missed ${r.missed_occurrences} earlier occurrence(s))`,
      );
    // Local time first when the server supplied one — `due_at` is a UTC instant
    // while `schedule` is wall clock in the server's zone, and pairing those two
    // leaves the agent unable to say when the reminder actually was.
    // `due_at_local` already carries the zone abbreviation the server renders
    // ("09:30 CEST"), so the IANA name is not repeated in the bullet — it stays
    // in the payload for an agent asked "which timezone is that schedule in?".
    if (r.due_at_local || r.due_at) {
      bits.push(`(due ${when(r.due_at_local, r.due_at)})`);
    }
    if (r.schedule) bits.push(`(${oneLine(r.schedule)})`);
    if (r.recurring)
      bits.push(
        r.next_due_at
          ? `(⟲ recurring — next fire ${when(r.next_due_at_local ?? undefined, r.next_due_at)})`
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
  // Rows are consumed as they are injected, so an overflow cannot be "shown
  // later" — it is stated, with the pointer that gets the rest.
  if (items.length > shown.length)
    lines.push(
      `…and ${items.length - shown.length} more consumed in this check — call list_reminders to see them.`,
    );
  return [
    "⏰ Due reminders from Alexandria (each of these is consumed now and will not be repeated unless it is marked recurring — act on it or tell the user):",
    ...lines,
  ].join("\n");
}
