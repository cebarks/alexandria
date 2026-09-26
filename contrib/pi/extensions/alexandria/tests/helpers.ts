/**
 * Shared helpers for the extension's tests.
 *
 * Not a test file itself — `tsx --test tests/` only collects `*.test.ts` — so
 * both the git-probe cases and the cwd juggling around them live here rather than
 * being copied into every file.
 */

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { after } from "node:test";

export const execFileAsync = promisify(execFile);

/** The extension package root — the directory holding `package.json`. */
export const extensionRoot = fileURLToPath(new URL("../", import.meta.url));

/** A path that is never created, so `config.ts` sees no client.toml on disk. */
export const absentClientToml = join(
	tmpdir(),
	"alexandria-extension-tests-absent-client.toml",
);

/**
 * The repository toplevel containing `cwd`, or undefined outside a repository.
 * Undefined also means "git is unusable here", which the probe-dependent tests
 * treat as a reason to skip: the probe under test is itself a git call.
 */
export async function gitToplevel(cwd: string): Promise<string | undefined> {
	try {
		const { stdout } = await execFileAsync(
			"git",
			["rev-parse", "--show-toplevel"],
			{ cwd },
		);
		return stdout.trim() === "" ? undefined : stdout.trim();
	} catch {
		return undefined;
	}
}

/**
 * A fresh scratch directory, removed when the calling file's tests finish — with
 * nothing asserted about git, because an OS temp dir inside a repository would
 * make a "not a repo" expectation meaningless. Callers check it with
 * {@linkcode gitToplevel}.
 */
export async function makeScratchDir(prefix: string): Promise<string> {
	const dir = await mkdtemp(join(tmpdir(), prefix));
	after(() => rm(dir, { recursive: true, force: true }));
	return dir;
}

/**
 * Run `probe` with the process cwd at `dir`. The hint probe takes no cwd, so the
 * process is the only way to place it; tests using this must stay sequential.
 */
export async function inDir<T>(
	dir: string,
	probe: () => Promise<T>,
): Promise<T> {
	const previous = process.cwd();
	process.chdir(dir);
	try {
		return await probe();
	} finally {
		process.chdir(previous);
	}
}
