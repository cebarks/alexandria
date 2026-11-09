import {
	accessSync,
	constants,
	closeSync,
	fchmodSync,
	fstatSync,
	existsSync,
	lstatSync,
	mkdirSync,
	openSync,
	readFileSync,
	realpathSync,
	renameSync,
	unlinkSync,
	writeFileSync,
} from "node:fs";
import { randomUUID } from "node:crypto";
import { basename, dirname, relative, resolve, sep } from "node:path";

export interface RecallSettings {
	enabled: boolean;
	limit: number;
	minSimilarity: number;
}

export type RecallSettingKey = keyof RecallSettings;

export interface RecallSettingsView {
	saved: RecallSettings;
	effective: RecallSettings;
	overrides: Partial<Record<RecallSettingKey, string>>;
}

export interface RecallSettingsSave extends RecallSettingsView {
	cleanupWarning?: string;
}

export interface RecallSettingsStore {
	get(): RecallSettingsView;
	save<K extends RecallSettingKey>(key: K, value: RecallSettings[K]): RecallSettingsSave;
}

type JsonObject = Record<string, unknown>;

function isObject(value: unknown): value is JsonObject {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}

function readInitial(path: string): JsonObject {
	try {
		const value: unknown = JSON.parse(readFileSync(path, "utf8"));
		return isObject(value) ? value : {};
	} catch {
		return {};
	}
}

function resolveView(document: JsonObject, environment: NodeJS.ProcessEnv): RecallSettingsView {
	const recall = isObject(document.recall) ? document.recall : {};
	const saved: RecallSettings = {
		enabled: recall.enabled !== false,
		limit: (recall.limit ?? 10) as number,
		minSimilarity: (recall.min_similarity ?? 0.45) as number,
	};
	const effective = { ...saved };
	const overrides: RecallSettingsView["overrides"] = {};
	const toggle = environment.ALEXANDRIA_AUTO_RECALL?.trim();
	if (toggle) {
		effective.enabled = toggle !== "off";
		overrides.enabled = "ALEXANDRIA_AUTO_RECALL";
	}
	for (const [key, name] of [
		["limit", "ALEXANDRIA_AUTO_RECALL_LIMIT"],
		["minSimilarity", "ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY"],
	] as const) {
		const raw = environment[name]?.trim();
		if (raw && Number.isFinite(Number(raw))) {
			effective[key] = Number(raw);
			overrides[key] = name;
		}
	}
	return { saved, effective, overrides };
}

function validateSetting(key: RecallSettingKey, value: boolean | number): void {
	if (key === "enabled" && typeof value === "boolean") return;
	if (key === "limit" && typeof value === "number" && Number.isInteger(value) && value >= 1 && value <= 100) return;
	if (key === "minSimilarity" && typeof value === "number" && Number.isFinite(value) && value >= -1 && value <= 1) return;
	throw new Error("Recall requires enabled true/false, a limit from 1 to 100, or a similarity from -1 to 1");
}

function ensureSafeNumbers(value: unknown): void {
	if (typeof value === "number" && (!Number.isFinite(value) || (Number.isInteger(value) && !Number.isSafeInteger(value)))) {
		throw new Error("Config contains a number that cannot be safely represented in JSON");
	}
	if (Array.isArray(value)) {
		for (const item of value) ensureSafeNumbers(item);
	} else if (isObject(value)) {
		for (const item of Object.values(value)) ensureSafeNumbers(item);
	}
}

function decimalForm(token: string): string {
	const match = /^(-?)(0|[1-9]\d*)(?:\.(\d+))?(?:[eE]([+-]?\d+))?$/.exec(token);
	if (!match) throw new Error("Invalid JSON number");
	const fraction = match[3] ?? "";
	let digits = `${match[2]}${fraction}`.replace(/^0+/, "");
	if (!digits) return match[1] ? "-0" : "0";
	const trailing = digits.match(/0+$/)?.[0].length ?? 0;
	if (trailing) digits = digits.slice(0, -trailing);
	const exponent = BigInt(match[4] ?? "0") - BigInt(fraction.length) + BigInt(trailing);
	return `${match[1]}${digits}e${exponent}`;
}

function ensureRoundTripSafe(json: string): void {
	const pattern = /-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/y;
	const scopes: (Set<string> | null)[] = [];
	for (let i = 0; i < json.length;) {
		if (json[i] === '"') {
			const start = i++;
			while (i < json.length) {
				if (json[i] === "\\") i += 2;
				else if (json[i++] === '"') break;
			}
			let next = i;
			while (/\s/.test(json[next] ?? "")) next++;
			if (json[next] === ":") {
				const key: string = JSON.parse(json.slice(start, i));
				const keys = scopes[scopes.length - 1];
				if (!keys || keys.has(key)) throw new Error("Config contains duplicate object keys");
				keys.add(key);
			}
		} else if (json[i] === "{") {
			scopes.push(new Set()); i++;
		} else if (json[i] === "[") {
			scopes.push(null); i++;
		} else if (json[i] === "}" || json[i] === "]") {
			scopes.pop(); i++;
		} else if (json[i] === "-" || (json[i] >= "0" && json[i] <= "9")) {
			pattern.lastIndex = i;
			const match = pattern.exec(json);
			if (!match) throw new Error("Invalid JSON number");
			const n = Number(match[0]);
			const emitted = JSON.stringify(n);
			if (!Number.isFinite(n) || typeof emitted !== "string" || decimalForm(match[0]) !== decimalForm(emitted)) {
				throw new Error("Config contains a number that would lose precision");
			}
			i = pattern.lastIndex;
		} else i++;
	}
}

function targetPath(path: string): string {
	const absolute = resolve(path);
	try {
		const stat = lstatSync(absolute);
		if (stat.isSymbolicLink()) return realpathSync(absolute);
		if (!stat.isFile()) throw new Error(`Config is not a regular file: ${absolute}`);
		return absolute;
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT" && !existsSync(absolute)) {
			// existsSync follows symlinks, so distinguish a dangling link before creating.
			try {
				if (lstatSync(absolute).isSymbolicLink()) throw new Error(`Config symlink is dangling: ${absolute}`);
			} catch (checkError) {
				if ((checkError as NodeJS.ErrnoException).code !== "ENOENT") throw checkError;
			}
			return absolute;
		}
		throw error;
	}
}

function checkTrustedDirectories(path: string): void {
	const uid = process.getuid?.();
	if (uid === undefined) throw new Error("Cannot verify directory ownership on this platform");
	let current = path;
	for (;;) {
		const info = lstatSync(current);
		if (!info.isDirectory() || (info.uid !== uid && info.uid !== 0)) {
			throw new Error(`Untrusted config directory: ${current}`);
		}
		if ((info.mode & 0o022) !== 0 && (info.mode & 0o1000) === 0) {
			throw new Error(`Config directory is writable by other users: ${current}`);
		}
		const parent = dirname(current);
		if (parent === current) return;
		current = parent;
	}
}

function trustedTarget(target: string): string {
	const requestedParent = dirname(target);
	let existing = requestedParent;
	let realParent: string;
	for (;;) {
		try {
			realParent = realpathSync(existing);
			break;
		} catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
			const parent = dirname(existing);
			if (parent === existing) throw error;
			existing = parent;
		}
	}
	checkTrustedDirectories(realParent);
	for (const segment of relative(existing, requestedParent).split(sep).filter(Boolean)) {
		const child = resolve(realParent, segment);
		try { mkdirSync(child, { mode: 0o700 }); } catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
		}
		checkTrustedDirectories(child);
		realParent = child;
	}
	return resolve(realParent, basename(target));
}

function readCurrent(path: string): string | undefined {
	try {
		return readFileSync(path, "utf8");
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
		throw error;
	}
}

function writeSetting(path: string, key: RecallSettingKey, value: boolean | number): { document: JsonObject; cleanupWarning?: string } {
	validateSetting(key, value);
	const target = trustedTarget(targetPath(path));
	const lock = `${target}.lock`;
	let lockFd: number;
	try {
		lockFd = openSync(lock, "wx", 0o600);
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "EEXIST") {
			throw new Error(`Config is locked: ${lock}. Check for another writer before removing a stale lock.`);
		}
		throw error;
	}
	const temp = `${target}.${randomUUID()}.tmp`;
	let committed: JsonObject | undefined;
	let failure: unknown;
	const cleanupErrors: string[] = [];
	try {
		writeFileSync(lockFd, `${process.pid}\n`);
		const original = readCurrent(target);
		const originalStat = original === undefined ? undefined : lstatSync(target);
		if (originalStat) {
			if (!originalStat.isFile()) throw new Error("Config is not a regular file");
			if (originalStat.nlink !== 1) throw new Error("Config has multiple hard links");
			if ((originalStat.mode & 0o222) === 0) throw new Error("Config is not writable");
			try { accessSync(target, constants.W_OK); } catch {
				throw new Error("Config is not writable");
			}
		}
		const document: unknown = original === undefined ? {} : JSON.parse(original);
		if (!isObject(document)) throw new Error("Config must be a JSON object");
		ensureSafeNumbers(document);
		if (original !== undefined) ensureRoundTripSafe(original);
		if (document.recall !== undefined && !isObject(document.recall)) {
			throw new Error("Config recall must be a JSON object");
		}
		const recall = isObject(document.recall) ? document.recall : {};
		recall[key === "minSimilarity" ? "min_similarity" : key] = value;
		document.recall = recall;
		const mode = originalStat ? originalStat.mode & 0o777 : 0o600;
		const fd = openSync(temp, "wx", mode);
		try {
			writeFileSync(fd, `${JSON.stringify(document, null, 2)}\n`);
			fchmodSync(fd, mode);
			const entry = lstatSync(temp);
			const opened = fstatSync(fd);
			if (!entry.isFile() || entry.dev !== opened.dev || entry.ino !== opened.ino || opened.nlink !== 1) {
				throw new Error("Config temporary file changed during the update");
			}
			if ((opened.mode & 0o777) !== mode) throw new Error("Config file mode cannot be preserved");
			if (trustedTarget(targetPath(path)) !== target) throw new Error("Config symlink changed during the update; try again");
			if (readCurrent(target) !== original) throw new Error("Config changed during the update; try again");
			if (originalStat) {
				const current = lstatSync(target);
				if (!current.isFile() || current.dev !== originalStat.dev || current.ino !== originalStat.ino ||
					current.uid !== originalStat.uid || current.gid !== originalStat.gid || current.mode !== originalStat.mode ||
					current.nlink !== originalStat.nlink) {
					throw new Error("Config changed during the update; try again");
				}
			} else {
				try {
					lstatSync(target);
					throw new Error("Config changed during the update; try again");
				} catch (error) {
					if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
				}
			}
			renameSync(temp, target);
			committed = document;
		} finally {
			try { closeSync(fd); } catch (error) { cleanupErrors.push(String(error)); }
		}
	} catch (error) {
		failure = error;
	} finally {
		try { unlinkSync(temp); } catch (error) {
			if ((error as NodeJS.ErrnoException).code !== "ENOENT") cleanupErrors.push(String(error));
		}
		try { closeSync(lockFd); } catch (error) { cleanupErrors.push(String(error)); }
		try { unlinkSync(lock); } catch (error) { cleanupErrors.push(String(error)); }
	}
	if (!committed) {
		const detail = failure instanceof Error ? failure.message : String(failure);
		const cleanup = cleanupErrors.length ? `; cleanup failed: ${cleanupErrors.join("; ")}` : "";
		throw new Error(`${detail}${cleanup}`);
	}
	if (failure) cleanupErrors.push(String(failure));
	const cleanupWarning = cleanupErrors.length
		? `Settings saved, but cleanup failed (${cleanupErrors.join("; ")}). If ${lock} remains, check that no writer is active before removing the stale lock.`
		: undefined;
	return { document: committed, cleanupWarning };
}

export function createRecallSettingsStore(
	path: string,
	environment: NodeJS.ProcessEnv = process.env,
	initial?: unknown,
): RecallSettingsStore {
	const env = { ...environment };
	let state = resolveView(isObject(initial) ? initial : readInitial(path), env);
	return {
		get() {
			return { saved: { ...state.saved }, effective: { ...state.effective }, overrides: { ...state.overrides } };
		},
		save(key, value) {
			const { document, cleanupWarning } = writeSetting(path, key, value);
			state = resolveView(document, env);
			const view = this.get();
			return cleanupWarning ? { ...view, cleanupWarning } : view;
		},
	};
}
