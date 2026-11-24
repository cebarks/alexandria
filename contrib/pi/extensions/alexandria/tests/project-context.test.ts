import { after, test } from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";

const managedEnv = ["ALEXANDRIA_CLIENT_CONFIG", "ALEXANDRIA_PROJECT_ID", "ALEXANDRIA_API_TOKEN", "GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_PREFIX"];
const previousEnv = new Map(managedEnv.map((key) => [key, process.env[key]] as const));
process.env.ALEXANDRIA_CLIENT_CONFIG = "/tmp/alexandria-tests-absent-client.toml";
for (const key of managedEnv) {
	if (key !== "ALEXANDRIA_CLIENT_CONFIG") delete process.env[key];
}

const { getProjectContext, __resetProjectContext } = await import("../src/project-context.js");
after(() => {
	for (const [key, value] of previousEnv) {
		if (value === undefined) delete process.env[key];
		else process.env[key] = value;
	}
});
const execFileAsync = promisify(execFile);

async function scratch(): Promise<string> {
	const dir = await mkdtemp(join(tmpdir(), "alexandria-project-context-"));
	after(() => rm(dir, { recursive: true, force: true }));
	return dir;
}

async function repo(remote: string): Promise<string> {
	const cwd = await scratch();
	await execFileAsync("git", ["init", "--quiet", cwd]);
	await execFileAsync("git", ["-C", cwd, "remote", "add", "origin", remote]);
	return cwd;
}

function server(accountKey = "account-a", serverKey = "https://memory.example/mcp") {
	const calls: { name: string; args: Record<string, unknown> }[] = [];
	return {
		calls,
		server: {
			accountKey,
			serverKey,
			callTool: async (name: string, args: Record<string, unknown>) => {
				calls.push({ name, args });
				return { content: [{ type: "text", text: JSON.stringify({ status: "ok", project: { id: "project:stable" } }) }] };
			},
		},
	};
}

const context = (cwd: string) => ({ cwd }) as never;

test("SSH/HTTPS clones and a git worktree resolve the same repository identity", async () => {
	const ssh = await repo("git@github.com:anten/alexandria.git");
	await execFileAsync("git", ["-C", ssh, "config", "user.name", "fixture"]);
	await execFileAsync("git", ["-C", ssh, "config", "user.email", "fixture@example.test"]);
	await execFileAsync("git", ["-C", ssh, "commit", "--quiet", "--allow-empty", "-m", "fixture"]);
	const worktreeParent = await scratch();
	const worktree = join(worktreeParent, "worktree");
	await execFileAsync("git", ["-C", ssh, "worktree", "add", "--quiet", "--detach", worktree, "HEAD"]);
	const https = await repo("https://github.com/anten/alexandria");
	const first = server();
	const second = server();
	const third = server();
	__resetProjectContext();
	const a = await getProjectContext(context(ssh), first.server);
	await getProjectContext(context(ssh), first.server);
	assert.equal(first.calls.length, 1, "same account/server/directory/descriptor should reuse the resolution");
	const worktreeResult = await getProjectContext(context(worktree), second.server);
	const b = await getProjectContext(context(https), third.server);
	assert.deepEqual([a.projectId, worktreeResult.projectId, b.projectId], ["project:stable", "project:stable", "project:stable"]);
	const remotes = [first, second, third].map((entry) => entry.calls[0].args.remote as Record<string, unknown>);
	for (const remote of remotes) {
		assert.deepEqual(
			{ host: remote.host, port: remote.port, path: remote.path },
			{ host: "github.com", port: undefined, path: "anten/alexandria" },
		);
	}
	assert.equal(remotes[0].transport, "ssh");
	assert.equal(remotes[1].transport, "ssh");
	assert.equal(remotes[2].transport, "https");
});

test("git probes ignore inherited GIT_* overrides", async () => {
	const cwd = await repo("git@github.com:anten/alexandria.git");
	const other = await repo("git@github.com:wrong/other.git");
	const prior = process.env.GIT_DIR;
	process.env.GIT_DIR = join(other, ".git");
	process.env.GIT_WORK_TREE = other;
	process.env.GIT_COMMON_DIR = join(other, ".git");
	__resetProjectContext();
	const target = server();
	try {
		await getProjectContext(context(cwd), target.server);
		assert.equal((target.calls[0].args.remote as { path: string }).path, "anten/alexandria");
	} finally {
		if (prior === undefined) delete process.env.GIT_DIR;
		else process.env.GIT_DIR = prior;
		delete process.env.GIT_WORK_TREE;
		delete process.env.GIT_COMMON_DIR;
	}
});

test("a repository without an origin stays Personal-only and sends no descriptor", async () => {
	const cwd = await scratch();
	await execFileAsync("git", ["init", "--quiet", cwd]);
	__resetProjectContext();
	const target = server();
	const result = await getProjectContext(context(cwd), target.server);
	assert.equal(result.projectId, undefined);
	assert.equal(result.source, "personal-only");
	assert.deepEqual(target.calls, []);
});

test("URL userinfo, query, and fragment secrets never reach resolve_project", async () => {
	const cwd = await repo("https://alice:password@example.test/anten/alexandria.git?token=secret#fragment-secret");
	__resetProjectContext();
	const target = server();
	await getProjectContext(context(cwd), target.server);
	const wire = JSON.stringify(target.calls[0].args);
	for (const secret of ["alice", "password", "token", "secret", "fragment-secret"]) assert.doesNotMatch(wire, new RegExp(secret, "i"));
	assert.deepEqual(target.calls[0].args.remote, {
		host: "example.test",
		path: "anten/alexandria",
		transport: "https",
	});
});

test("explicit project override resolves by handle without probing a remote", async () => {
	process.env.ALEXANDRIA_PROJECT_ID = "project:chosen";
	const cwd = await scratch();
	__resetProjectContext();
	const target = server();
	try {
		const result = await getProjectContext(context(cwd), target.server);
		assert.equal(result.projectId, "project:stable");
		assert.equal(result.source, "override");
		assert.deepEqual(target.calls[0].args, { project: "project:chosen" });
	} finally {
		delete process.env.ALEXANDRIA_PROJECT_ID;
	}
});

test("a failed project resolution is not converted into an unscoped context", async () => {
	const cwd = await repo("https://github.com/anten/alexandria");
	__resetProjectContext();
	const failed = server();
	failed.server.callTool = async (name, args) => {
		failed.calls.push({ name, args });
		return { isError: true, content: [{ type: "text", text: "authentication refused" }] };
	};
	await assert.rejects(getProjectContext(context(cwd), failed.server), /refusing an unscoped fallback/);
	assert.equal(failed.calls.length, 1);
});

test("switching account or server invalidates the project cache", async () => {
	const cwd = await repo("git@github.com:anten/alexandria.git");
	__resetProjectContext();
	const first = server("account-a", "https://one.example/mcp");
	const switchedAccount = server("account-b", "https://one.example/mcp");
	const switchedServer = server("account-b", "https://two.example/mcp");
	await getProjectContext(context(cwd), first.server);
	await getProjectContext(context(cwd), switchedAccount.server);
	await getProjectContext(context(cwd), switchedServer.server);
	assert.equal(first.calls.length, 1);
	assert.equal(switchedAccount.calls.length, 1);
	assert.equal(switchedServer.calls.length, 1);
});
