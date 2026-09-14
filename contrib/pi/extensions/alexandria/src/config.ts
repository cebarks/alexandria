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

// Trimmed at the boundary: a set-but-blank or whitespace-only override must fall
// through to the file (and then to the git probe) rather than being sent verbatim,
// because the server matches the target exactly and an untrimmed value degrades
// project delivery to escalation-only with no signal.
const envProject = process.env.ALEXANDRIA_REMINDERS_PROJECT?.trim();

/** Centralized configuration — TOML file with env var overrides. */
export const CONFIG = {
	serverUrl:
		process.env.ALEXANDRIA_URL ?? toml.server?.url ?? "http://127.0.0.1:3000/mcp",

	recallDisabled:
		process.env.ALEXANDRIA_AUTO_RECALL === "off" ||
		(toml.recall?.enabled === false &&
			process.env.ALEXANDRIA_AUTO_RECALL === undefined),

	recallLimit: Number(
		process.env.ALEXANDRIA_AUTO_RECALL_LIMIT ?? toml.recall?.limit ?? 5,
	),

	recallMinSimilarity: Number(
		process.env.ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY ??
			toml.recall?.min_similarity ??
			0.58,
	),

	storeDisabled:
		process.env.ALEXANDRIA_AUTO_STORE === "off" ||
		(toml.store?.enabled === false &&
			process.env.ALEXANDRIA_AUTO_STORE === undefined),

	extractModel:
		process.env.ALEXANDRIA_EXTRACT_MODEL ??
		toml.store?.extract_model ??
		"vertex/claude-haiku-4-5",

	extractTimeoutMs: Number(
		process.env.ALEXANDRIA_EXTRACT_TIMEOUT_MS ??
			toml.store?.extract_timeout_ms ??
			5000,
	),

	remindersDisabled:
		process.env.ALEXANDRIA_REMINDERS === "off" ||
		(toml.reminders?.enabled === false &&
			process.env.ALEXANDRIA_REMINDERS === undefined),

	// Exact, case-sensitive match against the server-side reminder target, so the
	// env override exists for contexts where the git toplevel basename is not the
	// project name (git worktrees, copied checkouts).
	remindersProject: envProject || toml.reminders?.project?.trim() || undefined,
} as const;
