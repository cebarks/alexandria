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
	transport?: { prompt_call_timeout_ms?: number; prompt_budget_ms?: number };
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

const obsoleteExtractionSettings = [
	...(process.env.ALEXANDRIA_AUTO_STORE !== undefined ? ["ALEXANDRIA_AUTO_STORE"] : []),
	...(process.env.ALEXANDRIA_EXTRACT_MODEL !== undefined ? ["ALEXANDRIA_EXTRACT_MODEL"] : []),
	...(process.env.ALEXANDRIA_EXTRACT_TIMEOUT_MS !== undefined ? ["ALEXANDRIA_EXTRACT_TIMEOUT_MS"] : []),
	...(config.store?.enabled !== undefined ? ["store.enabled in client.json"] : []),
	...(config.store?.extract_model !== undefined ? ["store.extract_model in client.json"] : []),
	...(config.store?.extract_timeout_ms !== undefined ? ["store.extract_timeout_ms in client.json"] : []),
];
if (obsoleteExtractionSettings.length > 0) {
	console.warn(
		`Alexandria: automatic observation storage was removed; ignoring obsolete setting(s) ${obsoleteExtractionSettings.join(", ")}. Remove them and use the authenticated store_memory tool or the Alexandria memory skill to save memories explicitly.`,
	);
}

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

function transportTimeouts(): { promptCallTimeoutMs: number; promptBudgetMs: number } {
	const configured = (envKey: string, fileValue: number | undefined, fallback: number): number => {
		const rawEnv = process.env[envKey];
		const value = rawEnv !== undefined ? Number(rawEnv.trim()) : (fileValue ?? fallback);
		if (!Number.isFinite(value) || value <= 0) {
			throw new Error(
				`Alexandria config error: transport ${envKey} must be a positive finite number of milliseconds.`,
			);
		}
		return value;
	};
	const promptCallTimeoutMs = configured(
		"ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS",
		config.transport?.prompt_call_timeout_ms,
		5000,
	);
	const promptBudgetMs = configured(
		"ALEXANDRIA_PROMPT_BUDGET_MS",
		config.transport?.prompt_budget_ms,
		10000,
	);
	if (promptBudgetMs > 120000) {
		throw new Error("Alexandria config error: transport prompt_budget_ms must be at most 120000 ms.");
	}
	if (promptCallTimeoutMs > promptBudgetMs) {
		throw new Error(
			"Alexandria config error: transport prompt_call_timeout_ms must be less than or equal to prompt_budget_ms.",
		);
	}
	return { promptCallTimeoutMs, promptBudgetMs };
}

const transport = transportTimeouts();

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
	get serverUrl(): string {
		return env("ALEXANDRIA_URL") ?? config.server?.url ?? "http://127.0.0.1:3000/mcp";
	},

	// Companion auth is intentionally independent of pi's/model providers' credentials.
	get apiToken(): string | undefined {
		return env("ALEXANDRIA_API_TOKEN");
	},

	// Read at call time so a live env change is seen without routing credentials
	// through any model-provider configuration.
	get projectId(): string | undefined {
		return env("ALEXANDRIA_PROJECT_ID");
	},

	get recallDisabled() { return !RECALL_SETTINGS.get().effective.enabled; },
	get recallLimit() { return RECALL_SETTINGS.get().effective.limit; },
	get recallMinSimilarity() { return RECALL_SETTINGS.get().effective.minSimilarity; },

	remindersDisabled:
		env("ALEXANDRIA_REMINDERS") === "off" ||
		(config.reminders?.enabled === false && env("ALEXANDRIA_REMINDERS") === undefined),

	// Exact, case-sensitive match against the server-side reminder target, so the
	// env override exists for contexts where the git toplevel basename is not the
	// project name (git worktrees, copied checkouts).
	remindersProject:
		env("ALEXANDRIA_REMINDERS_PROJECT") ?? config.reminders?.project?.trim() ?? undefined,

	promptCallTimeoutMs: transport.promptCallTimeoutMs,
	promptBudgetMs: transport.promptBudgetMs,
} as const;
