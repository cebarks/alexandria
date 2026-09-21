import { parse } from "smol-toml";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { homedir, platform } from "node:os";

interface ClientToml {
	server?: { url?: string };
	recall?: { enabled?: boolean; limit?: number; min_similarity?: number };
	store?: {
		enabled?: boolean;
		extract_model?: string;
		extract_timeout_ms?: number;
	};
	reminders?: { enabled?: boolean; project?: string };
}

/**
 * Platform-aware config directory, matching the Rust `dirs::config_dir()` behavior:
 * - Linux:  $XDG_CONFIG_HOME or ~/.config
 * - macOS:  ~/Library/Application Support
 * - Windows: %APPDATA% (not expected, but handled)
 */
function configDir(): string {
	if (process.env.XDG_CONFIG_HOME) return process.env.XDG_CONFIG_HOME;
	const home = homedir();
	switch (platform()) {
		case "darwin":
			return join(home, "Library", "Application Support");
		case "win32":
			return process.env.APPDATA ?? join(home, "AppData", "Roaming");
		default:
			return join(home, ".config");
	}
}

function loadToml(): ClientToml {
	const configPath =
		process.env.ALEXANDRIA_CLIENT_CONFIG ??
		join(configDir(), "alexandria", "client.toml");

	if (!existsSync(configPath)) return {};

	try {
		const raw = readFileSync(configPath, "utf-8");
		return parse(raw) as ClientToml;
	} catch (err) {
		console.warn(
			`Alexandria: failed to parse ${configPath}: ${err instanceof Error ? err.message : String(err)}; using defaults`,
		);
		return {};
	}
}

const toml = loadToml();

/**
 * Read an env override the same way everywhere: trimmed, and a set-but-blank
 * value treated as *unset*. A blank `ALEXANDRIA_REMINDERS=` (a direnv or `.env`
 * habit) is not a user saying "off", and gating the file branch on
 * `process.env.X === undefined` would let it re-enable a feature the user
 * disabled in `client.toml` — silently turning the consuming per-prompt
 * `check_reminders` call back on.
 */
const env = (key: string): string | undefined => process.env[key]?.trim() || undefined;

/** A numeric override, or the fallback. A blank or unparseable value falls back
 *  rather than becoming `0` or `NaN`: `Number("")` is `0`, which would silence
 *  recall entirely, and `NaN` compares false against every similarity, which
 *  looks identical to "nothing matched". */
const envNum = (key: string, fallback: number): number => {
	const raw = env(key);
	if (raw === undefined) return fallback;
	const n = Number(raw);
	if (!Number.isFinite(n)) {
		console.warn(`Alexandria: ignoring non-numeric ${key}=${JSON.stringify(raw)}; using ${fallback}`);
		return fallback;
	}
	return n;
};

/** Trimmed at the boundary: a set-but-blank or whitespace-only override must fall
 *  through to the file (and then to the git probe) rather than being sent verbatim,
 *  because the server matches the target exactly and an untrimmed value degrades
 *  project delivery to escalation-only with no signal.
 *  Centralized configuration — TOML file with env var overrides. */
export const CONFIG = {
	serverUrl: env("ALEXANDRIA_URL") ?? toml.server?.url ?? "http://127.0.0.1:3000/mcp",

	recallDisabled:
		env("ALEXANDRIA_AUTO_RECALL") === "off" ||
		(toml.recall?.enabled === false && env("ALEXANDRIA_AUTO_RECALL") === undefined),

	recallLimit: envNum(
		"ALEXANDRIA_AUTO_RECALL_LIMIT",
		// 10, matching the Claude Code hook. A judgement call read off the
		// 2026-09-09 bench-retrieval limit x threshold grid (880 facts, a small
		// hand-authored question set): delivery saturates at 10 because the worst
		// known target rank is 9, so 15 and 20 add non-targets and no hits. Paired
		// with the threshold below; changing one alone leaves the frontier.
		toml.recall?.limit ?? 10,
	),

	recallMinSimilarity: envNum(
		"ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY",
		toml.recall?.min_similarity ??
			// 0.45, matching the Claude Code hook. Same grid, same caveats: at
			// limit 10 it delivers 8 of 12 targets at ~1.0 non-targets per prompt,
			// where 5/0.35 delivered the same 8 at ~3.2. It is only on the frontier
			// because the limit is 10; at 5 it was dominated by 0.50.
			// See docs/minilm-test-data.md "Result limit".
			0.45,
	),

	storeDisabled:
		env("ALEXANDRIA_AUTO_STORE") === "off" ||
		(toml.store?.enabled === false && env("ALEXANDRIA_AUTO_STORE") === undefined),

	extractModel:
		env("ALEXANDRIA_EXTRACT_MODEL") ??
		toml.store?.extract_model ??
		"vertex/claude-haiku-4-5",

	extractTimeoutMs: envNum(
		"ALEXANDRIA_EXTRACT_TIMEOUT_MS",
		toml.store?.extract_timeout_ms ?? 5000,
	),

	remindersDisabled:
		env("ALEXANDRIA_REMINDERS") === "off" ||
		(toml.reminders?.enabled === false && env("ALEXANDRIA_REMINDERS") === undefined),

	// Exact, case-sensitive match against the server-side reminder target, so the
	// env override exists for contexts where the git toplevel basename is not the
	// project name (git worktrees, copied checkouts).
	remindersProject:
		env("ALEXANDRIA_REMINDERS_PROJECT") ?? toml.reminders?.project?.trim() ?? undefined,
} as const;
