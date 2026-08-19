/** Centralized configuration — all env var reads in one place. */
export const CONFIG = {
	serverUrl: process.env.ALEXANDRIA_URL ?? "http://127.0.0.1:3000/mcp",
	recallDisabled: process.env.ALEXANDRIA_AUTO_RECALL === "off",
	recallLimit: Number(process.env.ALEXANDRIA_AUTO_RECALL_LIMIT ?? "5"),
	recallMinSimilarity: Number(process.env.ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY ?? "0.5"),
	storeDisabled: process.env.ALEXANDRIA_AUTO_STORE === "off",
	extractModel: process.env.ALEXANDRIA_EXTRACT_MODEL ?? "vertex/claude-haiku-4-5",
	extractTimeoutMs: Number(process.env.ALEXANDRIA_EXTRACT_TIMEOUT_MS ?? "5000"),
} as const;
