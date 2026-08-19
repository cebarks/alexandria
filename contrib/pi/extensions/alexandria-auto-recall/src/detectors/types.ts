/** A memory candidate detected by a heuristic or the LLM extraction pass. */
export interface DetectedMemory {
	content: string;
	tags: string[];
}

/**
 * Session-scoped dedup buffer.
 * Tracks what has been stored this session (by both heuristics and agent tool calls)
 * to prevent the LLM extraction pass from re-extracting known facts.
 */
export class SessionDedupBuffer {
	private readonly normalizedStrings = new Set<string>();
	private readonly contentHashes = new Set<string>();
	private readonly rawContents: string[] = [];

	private normalize(s: string): string {
		return s.toLowerCase().replace(/\s+/g, " ").trim();
	}

	private async hash(s: string): Promise<string> {
		const data = new TextEncoder().encode(s);
		const buf = await crypto.subtle.digest("SHA-256", data);
		return Array.from(new Uint8Array(buf))
			.map((b) => b.toString(16).padStart(2, "0"))
			.join("");
	}

	/** Record a heuristic-detected store (normalized string dedup). Returns false if duplicate. */
	addHeuristicStore(content: string): boolean {
		const norm = this.normalize(content);
		if (this.normalizedStrings.has(norm)) return false;
		this.normalizedStrings.add(norm);
		this.rawContents.push(content);
		return true;
	}

	/** Record an agent-initiated store_memory call (content hash dedup). */
	async addToolStore(content: string): Promise<void> {
		const h = await this.hash(content);
		this.contentHashes.add(h);
		this.rawContents.push(content);
	}

	/** Check if content was already stored by a heuristic this session. */
	hasHeuristic(content: string): boolean {
		return this.normalizedStrings.has(this.normalize(content));
	}

	/** Get all raw content strings for the extraction prompt's "already stored" section. */
	getAllStoredContents(): readonly string[] {
		return this.rawContents;
	}
}
