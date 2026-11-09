import { readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir, platform } from "node:os";
import { createRecallSettingsStore } from "./recall-config.js";

export { createRecallSettingsStore } from "./recall-config.js";

interface ClientJson {
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

const configuredPath = process.env.ALEXANDRIA_CLIENT_CONFIG;
const configPath = configuredPath?.trim()
	? configuredPath
	: join(configDir(), "alexandria", "client.json");

function loadJson(): ClientJson {
	try {
		const raw = readFileSync(configPath, "utf-8");
		const parsed: unknown = JSON.parse(raw);
		if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
			throw new Error("expected a JSON object");
		}
		return parsed as ClientJson;
	} catch (err) {
		if ((err as NodeJS.ErrnoException).code === "ENOENT") return {};
		console.warn(
			`Alexandria: failed to read or parse ${configPath}: ${err instanceof Error ? err.message : String(err)}; using defaults`,
		);
		return {};
	}
}

const config = loadJson();

/**
 * Read an env override the same way everywhere: trimmed, and a set-but-blank
 * value treated as *unset*. A blank `ALEXANDRIA_REMINDERS=` (a direnv or `.env`
 * habit) is not a user saying "off", and gating the file branch on
 * `process.env.X === undefined` would let it re-enable a feature the user
 * disabled in `client.json` — silently turning the consuming per-prompt
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

// Keep the existing warnings for malformed numeric overrides at startup.
void envNum("ALEXANDRIA_AUTO_RECALL_LIMIT", config.recall?.limit ?? 10);
void envNum("ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY", config.recall?.min_similarity ?? 0.45);
export const RECALL_SETTINGS = createRecallSettingsStore(configPath, process.env, config);

/** Trimmed at the boundary: a set-but-blank or whitespace-only override must fall
 *  through to the file (and then to the git probe) rather than being sent verbatim,
 *  because the server matches the target exactly and an untrimmed value degrades
 *  project delivery to escalation-only with no signal.
 *  Centralized configuration — JSON file with env var overrides. */
export const CONFIG = {
	serverUrl: env("ALEXANDRIA_URL") ?? config.server?.url ?? "http://127.0.0.1:3000/mcp",

	get recallDisabled() { return !RECALL_SETTINGS.get().effective.enabled; },
	get recallLimit() { return RECALL_SETTINGS.get().effective.limit; },
	get recallMinSimilarity() { return RECALL_SETTINGS.get().effective.minSimilarity; },

	storeDisabled:
		env("ALEXANDRIA_AUTO_STORE") === "off" ||
		(config.store?.enabled === false && env("ALEXANDRIA_AUTO_STORE") === undefined),

	extractModel:
		env("ALEXANDRIA_EXTRACT_MODEL") ??
		config.store?.extract_model ??
		"vertex/claude-haiku-4-5",

	extractTimeoutMs: envNum(
		"ALEXANDRIA_EXTRACT_TIMEOUT_MS",
		config.store?.extract_timeout_ms ?? 5000,
	),

	remindersDisabled:
		env("ALEXANDRIA_REMINDERS") === "off" ||
		(config.reminders?.enabled === false && env("ALEXANDRIA_REMINDERS") === undefined),

	// Exact, case-sensitive match against the server-side reminder target, so the
	// env override exists for contexts where the git toplevel basename is not the
	// project name (git worktrees, copied checkouts).
	remindersProject:
		env("ALEXANDRIA_REMINDERS_PROJECT") ?? config.reminders?.project?.trim() ?? undefined,
} as const;
