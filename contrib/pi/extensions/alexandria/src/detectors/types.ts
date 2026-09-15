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
	private readonly rawContents: string[] = [];

	private normalize(s: string): string {
		return s.toLowerCase().replace(/\s+/g, " ").trim();
	}

	/** Record a heuristic-detected store (normalized string dedup). Returns false if duplicate. */
	addHeuristicStore(content: string): boolean {
		const norm = this.normalize(content);
		if (this.normalizedStrings.has(norm)) return false;
		this.normalizedStrings.add(norm);
		this.rawContents.push(content);
		return true;
	}

	/** Record an agent-initiated store_memory call. */
	addToolStore(content: string): void {
		this.rawContents.push(content);
	}

	/** Get all raw content strings for the extraction prompt's "already stored" section. */
	getAllStoredContents(): readonly string[] {
		return this.rawContents;
	}
}
