import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { CONFIG } from "./config.js";
import { extractTextContent, type AuthenticatedServer } from "./mcp-client.js";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";

const execFileAsync = promisify(execFile);
const GIT_ENV_OVERRIDES = [
	"GIT_DIR",
	"GIT_COMMON_DIR",
	"GIT_WORK_TREE",
	"GIT_INDEX_FILE",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
	"GIT_PREFIX",
] as const;
const cache = new Map<string, Promise<ProjectContext>>();
let activeIdentity: string | undefined;
let cacheGeneration = 0;

export interface ProjectContext {
	projectId?: string;
	source: string;
}

interface RemoteDescriptor {
	host: string;
	port?: number;
	path: string;
	transport: "https" | "ssh";
}

/** Parse only the identity-bearing pieces. Userinfo, query and fragment are discarded before a
 * descriptor is returned, serialized, or hashed. */
export function sanitizeRemote(value: string): RemoteDescriptor | undefined {
	let host: string;
	let port: number | undefined;
	let path: string;
	let transport: "https" | "ssh";
	try {
		if (/^https:\/\//i.test(value) || /^ssh:\/\//i.test(value)) {
			const url = new URL(value);
			transport = url.protocol === "https:" ? "https" : "ssh";
			host = url.hostname.toLowerCase();
			const numericPort = url.port ? Number(url.port) : undefined;
			port = numericPort && numericPort !== (transport === "https" ? 443 : 22) ? numericPort : undefined;
			path = url.pathname;
		} else {
			const scp = /^(?:[^@/:]+@)?([^/:]+):([^?#]+)(?:[?#].*)?$/.exec(value);
			if (!scp) return undefined;
			transport = "ssh";
			host = scp[1].toLowerCase();
			path = scp[2];
		}
	} catch {
		return undefined;
	}
	path = path.replace(/^\/+|\/+$/g, "").replace(/\.git$/i, "");
	if (!host || !path || /[?#]/.test(path)) return undefined;
	return { host, ...(port === undefined ? {} : { port }), path, transport };
}

function sanitizedGitEnv(): NodeJS.ProcessEnv {
	const env = { ...process.env };
	for (const key of GIT_ENV_OVERRIDES) delete env[key];
	return env;
}

async function originRemote(cwd: string): Promise<string | undefined> {
	try {
		const { stdout } = await execFileAsync("git", ["remote", "get-url", "origin"], {
			cwd,
			timeout: 2000,
			env: sanitizedGitEnv(),
		});
		return stdout.trim() || undefined;
	} catch (err) {
		if ((err as { killed?: boolean }).killed) throw new Error("git origin lookup timed out");
		return undefined;
	}
}

function projectIdFromResult(result: { content?: unknown; isError?: boolean }): string {
	const text = extractTextContent(result.content);
	if (result.isError || !text) throw new Error("resolve_project failed; refusing an unscoped fallback");
	let payload: { status?: unknown; project?: { id?: unknown }; message?: unknown };
	try {
		payload = JSON.parse(text) as typeof payload;
	} catch {
		throw new Error("resolve_project returned an incompatible response; upgrade the Alexandria server");
	}
	if (typeof payload.project?.id === "string" && payload.project.id.startsWith("project:")) {
		return payload.project.id;
	}
	if (typeof payload.message === "string" && /tool|method|unknown|not found/i.test(payload.message)) {
		throw new Error("Alexandria server does not support resolve_project; upgrade the server before using the companion.");
	}
	throw new Error("resolve_project returned no issued project ID; refusing an unscoped fallback");
}

function identity(server: AuthenticatedServer): string {
	return `${server.serverKey}\u0000${server.accountKey}`;
}

/** Resolve an explicit project handle or the session cwd's sanitized origin. Without either, the
 * extension remains Personal-only and sends no guessed project selector. */
export async function getProjectContext(
	ctx: Pick<ExtensionContext, "cwd">,
	authenticatedServer: AuthenticatedServer,
	signal?: AbortSignal,
): Promise<ProjectContext> {
	const serverIdentity = identity(authenticatedServer);
	if (activeIdentity !== serverIdentity) {
		cache.clear();
		activeIdentity = serverIdentity;
		cacheGeneration++;
	}
	const generation = cacheGeneration;
	const cwd = ctx.cwd || process.cwd();
	const override = CONFIG.projectId;
	const remote = override ? undefined : await originRemote(cwd);
	const descriptor = remote ? sanitizeRemote(remote) : undefined;
	const source = override ? "override" : descriptor ? "git-remote" : "personal-only";
	const descriptorKey = override
		? `project:${override}`
		: descriptor
			? JSON.stringify({ host: descriptor.host, port: descriptor.port, path: descriptor.path })
			: "personal-only";
	const key = `${serverIdentity}\u0000${cwd}\u0000${descriptorKey}`;
	const existing = cache.get(key);
	if (existing) return existing;

	const pending = (async (): Promise<ProjectContext> => {
		if (!override && !descriptor) return { source };
		const args = override ? { project: override } : { remote: descriptor };
		const result = await authenticatedServer.callTool("resolve_project", args, 5000, signal);
		return { projectId: projectIdFromResult(result), source };
	})();
	if (generation === cacheGeneration) cache.set(key, pending);
	try {
		return await pending;
	} catch (error) {
		if (cache.get(key) === pending) cache.delete(key);
		throw error;
	} finally {
		if (generation !== cacheGeneration && cache.get(key) === pending) cache.delete(key);
	}
}

/** Clear memoized resolution for tests and account/server session transitions. */
export function __resetProjectContext(): void {
	cache.clear();
	activeIdentity = undefined;
	cacheGeneration++;
}
