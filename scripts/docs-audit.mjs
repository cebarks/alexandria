#!/usr/bin/env node
/**
 * Documentation audit for alexandria.
 *
 * Two modes:
 *
 *   node scripts/docs-audit.mjs            tiers 1+2, human-readable
 *   node scripts/docs-audit.mjs --claims   tier-3 claim inventory (JSON)
 *
 * Flags: --json (machine-readable findings), --tier 1|2 (restrict).
 *
 * Deliberately NOT wired into CI. It exists so "re-run the audit" is a real
 * option rather than a promise; the sweep that produced docs/audit-*.md used
 * it, and the next one should too.
 *
 * The design principle: derive ground truth from SOURCE, derive claims from
 * DOCS, and diff. A read-through finds what the reader thinks to question; a
 * diff finds everything that differs.
 *
 * Exits 1 when findings exist, so it can be gated later without being edited.
 */

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, normalize, resolve } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");

// ---------------------------------------------------------------- utilities

const read = (rel) => readFileSync(join(ROOT, rel), "utf8");
const sh = (cmd, args) =>
	execFileSync(cmd, args, { cwd: ROOT, encoding: "utf8", maxBuffer: 1 << 26 });

/** Every tracked markdown file the audit covers. */
function trackedDocs() {
	const all = sh("git", ["ls-files", "*.md"]).split("\n").filter(Boolean);
	return all.filter((f) => !isAuditReport(f));
}

/**
 * Audit reports are excluded from every check. They are working artifacts that
 * quote defective paths and wrong job lists *on purpose* — that is the finding —
 * so scanning one re-reports the defects it documents as if they were new.
 */
const isAuditReport = (f) => /^docs\/audit-\d{4}-\d{2}-\d{2}\.md$/.test(f);

/**
 * A *record* doc describes a past state on purpose: the two findings reports
 * ("Reading this after the audit date", with per-finding Status lines) and
 * `docs/prompt-path-stall-attribution.md` (a top-level STATUS banner naming
 * which tasks were not implemented). Backticked paths inside one name code that
 * was planned, prescribed, or has since been removed, so a missing path there is
 * advisory rather than a defect — reported as `info`, which does not fail the
 * run. Markdown links are still held to resolving, since those point at docs
 * rather than at code.
 */
function isRecordDoc(f) {
	if (!f.endsWith(".md") || !existsSync(join(ROOT, f))) return false;
	const head = read(f).split("\n").slice(0, 12).join("\n");
	return (
		/Reading this after the audit date/.test(head) ||
		/\*\*STATUS[^*]*(superseded|partial|historical)/i.test(head)
	);
}

/**
 * Docs that are maintained and therefore must appear in every doc index. Only
 * CLAUDE.md is excluded: it is an 11-byte `@AGENTS.md` import shim, not a
 * document. (`docs/plans/**` used to be excluded here as historical; that
 * directory was deleted, so every remaining tracked doc is maintained and has
 * to be indexed.)
 */
const INDEX_EXCLUDES = new Set(["CLAUDE.md"]);
const indexableDocs = () =>
	trackedDocs().filter((f) => !INDEX_EXCLUDES.has(f) && f !== "README.md");

const findings = [];
function report(tier, check, doc, detail, severity = "error") {
	findings.push({ tier, check, doc, detail, severity });
}

// ------------------------------------------------- truth: derived from source

/**
 * The advertised MCP tool set. `PUBLIC_TOOLS` in server.rs is authoritative —
 * `test_advertised_tool_set_is_exactly_the_thirteen_public_tools` compares it
 * against what the router actually advertises, so it cannot drift from the
 * server. Parsing it (a flat string array) is exact, unlike regex-matching
 * `#[tool(...)]` attributes, whose descriptions contain `]`.
 */
function truthTools() {
	const src = read("crates/alexandria-mcp/src/server.rs");
	const m = src.match(/PUBLIC_TOOLS[^=]*=\s*\[([\s\S]*?)\];/);
	if (!m) throw new Error("PUBLIC_TOOLS not found in server.rs");
	return [...m[1].matchAll(/"([^"]+)"/g)].map((x) => x[1]);
}

/** CLI subcommands, from the `match args[..]` arms in main.rs. */
function truthCli() {
	const src = read("crates/alexandria/src/main.rs");
	const body = src.match(/match args[\s\S]*?\n\t\}/);
	const region = body ? body[0] : src;
	const arms = [...region.matchAll(/\[([^\]]*)\]\s*=>/g)]
		.map((m) =>
			[...m[1].matchAll(/"([^"]+)"/g)]
				.map((x) => x[1])
				.filter((s) => s !== "--help" && s !== "-h"),
		)
		.filter((a) => a.length > 0)
		.map((a) => a.join(" "));
	return [...new Set(arms)];
}

/** Debug UI routes, from `router_with_context` in debug/mod.rs. */
function truthRoutes() {
	const src = read("crates/alexandria-mcp/src/debug/mod.rs");
	return [...src.matchAll(/\.route\("([^"]+)"/g)].map((m) => m[1]);
}

/**
 * Server config leaf keys, from config.rs. Section grouping is not preserved —
 * the defect class we care about is a key that exists in code and appears in no
 * doc, and a flat set answers that.
 */
function truthServerConfigKeys() {
	const src = read("crates/alexandria/src/config.rs");
	// Only fields inside structs, not local variables: `pub name: Type` at indent.
	return [...new Set([...src.matchAll(/^\s+pub (\w+):/gm)].map((m) => m[1]))];
}

/**
 * ALEXANDRIA_* env vars, scoped by reader. The server's vars and the pi
 * client's vars are different surfaces documented in different files, so they
 * are derived separately — lumping them reports server vars as "missing" from
 * the extension README, which should never mention them.
 */
function envVarsFrom(glob) {
	const files = sh("git", ["ls-files", glob]).split("\n").filter(Boolean);
	const vars = new Set();
	for (const f of files) {
		for (const m of read(f).matchAll(/ALEXANDRIA_[A-Z0-9_]+/g)) vars.add(m[0]);
	}
	return [...vars].sort();
}
const truthServerEnvVars = () => envVarsFrom("crates/**/*.rs");
const truthClientEnvVars = () => envVarsFrom("contrib/pi/**/*.ts");

/** just recipes. */
function truthJustRecipes() {
	const src = read("justfile");
	return [...src.matchAll(/^([a-z][a-z0-9_-]*)(?::[^=\n]*)?:(?!=)/gm)].map(
		(m) => m[1],
	);
}

/**
 * The recipes a doc must still name even when it points at bare `just` for the
 * full list: the ones `ci` depends on, read straight from the justfile so the
 * set cannot drift. Everything else is a convenience alias that bare `just`
 * surfaces, and enumerating it in prose is what makes a doc list rot.
 */
function loadBearingRecipes() {
	const src = read("justfile");
	const m = src.match(/^ci:([^\n]*)$/m);
	if (!m) throw new Error("no `ci:` recipe in justfile");
	const deps = m[1].trim().split(/\s+/).filter(Boolean);
	return new Set(["ci", ...deps]);
}

/**
 * CI jobs as `{key, name}`. Both spellings are legitimate in prose: AGENTS.md
 * says "a separate `Pi companion` job" (the display name) while ci.yml keys it
 * `extension`. Checking only the key reports correct prose as missing.
 */
function truthCiJobs() {
	const src = read(".github/workflows/ci.yml");
	const at = src.indexOf("\njobs:");
	if (at < 0) throw new Error("no jobs: block in ci.yml");
	const body = src.slice(at + "\njobs:".length);
	return [...body.matchAll(/^ {2}([a-zA-Z0-9_-]+):\n(?:.*\n)*? {4}name: (.+)$/gm)].map(
		(m) => ({ key: m[1], name: m[2].trim() }),
	);
}

/**
 * Client (pi extension) config keys, from the `ClientToml` interface in
 * config.ts. Returned as `section.key` plus bare `key` forms, because
 * docs/configuration.md documents them both ways.
 */
function truthClientConfigKeys() {
	const src = read("contrib/pi/extensions/alexandria/src/config.ts");
	const m = src.match(/interface ClientToml \{([\s\S]*?)\n\}/);
	if (!m) throw new Error("ClientToml interface not found");
	const keys = new Set();
	const walk = (text, prefix) => {
		for (const line of text.split("\n")) {
			const inline = line.match(/^\s*(\w+)\?\s*:\s*\{(.*)\}\s*;?\s*$/);
			if (inline) {
				walk(inline[2], `${prefix}${inline[1]}.`);
				continue;
			}
			const leaf = line.match(/^\s*(\w+)\?\s*:/);
			if (leaf) keys.add(`${prefix}${leaf[1]}`);
		}
	};
	walk(m[1], "");
	return [...keys].sort();
}

// ------------------------------------------------------- doc-side extraction

/** Strip the per-client MCP tool-name prefixes so one list serves all docs. */
function normalizeToolName(name) {
	return name
		.replace(/^mcp__alexandria__/, "")
		.replace(/^alexandria_/, "");
}

/**
 * The reverse direction: a doc that names a tool the server does not advertise.
 * Catches renamed and removed tools, which the forward check cannot — a doc
 * still describing `retrieve_memories_dry` as a tool looks complete.
 *
 * Crate names are excluded: `alexandria_engine` and `alexandria_storage` are
 * crates, not tools, and both appear in prose constantly.
 */
function checkPhantomTools(docs, tools) {
	const known = new Set(tools);
	const crates = new Set(
		sh("git", ["ls-files", "crates/*/Cargo.toml"])
			.split("\n")
			.filter(Boolean)
			.map((p) => p.split("/")[1].replace(/^alexandria-?/, "").replace(/-/g, "_"))
			.filter((c) => c && c !== "alexandria"),
	);
	for (const doc of docs) {
		const text = read(doc);
		const named = new Set();
		for (const m of text.matchAll(/\b(?:mcp__alexandria__|alexandria_)(\w+)\b/g)) {
			named.add(m[1]);
		}
		for (const n of named) {
			if (known.has(n) || crates.has(n)) continue;
			report(2, "phantom-tool", doc, `\`${n}\` is described as a tool but is not advertised by the server`);
		}
	}
}

/** Every backticked identifier-ish token in a doc. */
function backticked(doc) {
	return [...read(doc).matchAll(/`([^`\n]+)`/g)].map((m) => m[1]);
}

const escapeRe = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/**
 * Does `doc` mention `item`? Tries the forms documentation actually uses,
 * instead of a bare substring test (which reports `url` as absent from a file
 * full of `url = "..."`) or a length guard (which reports `ci` as absent from a
 * file containing `just ci`).
 */
function mentions(docText, docTokens, item, extraForms = []) {
	if (docTokens.has(item)) return true;
	const spellings = [item, ...extraForms].filter((s) => s && s.length > 1);
	for (const s of spellings) {
		const e = escapeRe(s);
		// identifier with word boundaries, respecting `-`/`_` as word chars
		if (new RegExp(`(?<![\\w-])${e}(?![\\w-])`).test(docText)) return true;
		// config-ish spellings: `key = ...`, `key: ...`, `just key`
		if (new RegExp(`(?<![\\w-])${e}\\s*[=:]`).test(docText)) return true;
		if (new RegExp(`just\\s+${e}(?![\\w-])`).test(docText)) return true;
	}
	return false;
}

/**
 * Entries of a markdown table's first column, or of a `- \`path\`` doc-map list.
 * Returns resolved repo-relative paths where they look like paths.
 */
function indexEntriesFrom(doc, mode) {
	const src = read(doc);
	const entries = new Set();
	if (mode === "table") {
		for (const m of src.matchAll(/^\|\s*\[([^\]]+)\]\(([^)]+)\)/gm)) {
			entries.add(m[2].replace(/\/$/, ""));
		}
	} else {
		for (const m of src.matchAll(/^-\s+`([^`]+)`/gm)) entries.add(m[1]);
	}
	return [...entries];
}

// --------------------------------------------------------------- tier 1

function checkLinks(docs) {
	for (const doc of docs) {
		const src = read(doc);
		const base = dirname(doc);
		for (const m of src.matchAll(/\]\(([^)\s]+)\)/g)) {
			let target = m[1];
			if (/^(https?:|mailto:|tel:|#)/.test(target)) continue;
			target = target.split("#")[0];
			if (!target) continue;
			const resolved = normalize(join(base, target));
			if (!existsSync(join(ROOT, resolved))) {
				report(1, "link", doc, `-> ${m[1]} (resolved ${resolved}) does not exist`);
			}
		}
	}
}

/**
 * GitHub's heading-anchor algorithm: lowercase, drop backticks and anything
 * that is not alphanumeric/space/hyphen, then spaces to hyphens.
 */
function anchorOf(heading) {
	return heading
		.trim()
		.toLowerCase()
		.replace(/`/g, "")
		.replace(/[^\w\s-]/g, "")
		.replace(/\s+/g, "-");
}

/** Anchors available in a doc, ignoring lines inside code fences. */
function anchorsIn(doc) {
	const set = new Set();
	let fence = false;
	for (const line of read(doc).split("\n")) {
		if (/^\s*(```|~~~)/.test(line)) {
			fence = !fence;
			continue;
		}
		if (fence) continue;
		const m = line.match(/^#{1,6}\s+(.+?)\s*$/);
		if (m) set.add(anchorOf(m[1]));
	}
	return set;
}

/**
 * Fragment links resolve. `checkLinks` strips `#...` before testing the path, so
 * without this a link to a renamed or deleted heading passes — the path is fine
 * and the reader lands at the top of the wrong document.
 */
function checkAnchors(docs) {
	const cache = new Map();
	const anchors = (d) => {
		if (!cache.has(d)) cache.set(d, existsSync(join(ROOT, d)) ? anchorsIn(d) : null);
		return cache.get(d);
	};
	for (const doc of docs) {
		const base = dirname(doc);
		for (const m of read(doc).matchAll(/\]\(([^)\s]+)\)/g)) {
			const target = m[1];
			if (/^(https?:|mailto:|tel:)/.test(target)) continue;
			const hash = target.indexOf("#");
			if (hash < 0) continue; // no fragment; checkLinks owns it
			const frag = target.slice(hash + 1);
			if (!frag) continue;
			const file = hash === 0 ? doc : normalize(join(base, target.slice(0, hash)));
			const set = anchors(file);
			if (set === null) continue; // missing file: checkLinks already reported it
			if (!set.has(frag)) {
				report(
					1,
					"anchor",
					doc,
					`-> ${target}: no heading in ${file} produces #${frag}`,
				);
			}
		}
	}
}

/**
 * Backticked repo-rooted paths must resolve. Elided paths (containing `...`)
 * are reported separately: they cannot resolve, and the findings-doc convention
 * is to cite symbols rather than paths, so the fix is a symbol cite.
 */
function checkBacktickedPaths(docs) {
	const rooted = /^(docs|crates|contrib|scripts|\.github)\//;
	const rootFiles = new Set([
		"justfile",
		"Cargo.toml",
		"Cargo.lock",
		"Containerfile",
		"deny.toml",
		"rustfmt.toml",
		"LICENSE",
	]);
	for (const doc of docs) {
		for (const raw of backticked(doc)) {
			const token = raw.trim();
			if (token.includes("*") || token.includes("<") || token.includes("{")) continue;
			const candidate = token.split(/[\s,;:)]/)[0].replace(/\.$/, "");
			if (!candidate) continue;

			const isRootFile = rootFiles.has(candidate);
			const isRooted = rooted.test(candidate);
			if (!isRootFile && !isRooted) continue;
			// A bare filename with no slash is prose ("the `justfile`"), not a path.
			if (!isRootFile && !candidate.includes("/")) continue;

			if (candidate.includes("...")) {
				report(
					1,
					"elided-path",
					doc,
					`\`${candidate}\` is elided and cannot resolve; cite a symbol instead`,
				);
				continue;
			}
			if (!existsSync(join(ROOT, candidate))) {
				report(
					1,
					"path",
					doc,
					`\`${candidate}\` does not exist`,
					isRecordDoc(doc) ? "info" : "error",
				);
			}
		}
	}
	// Cache nothing: isRecordDoc reads a 12-line head per call, and the set of docs
	// is small. Keeping it stateless avoids a stale cache across a merge.
}

/**
 * Both doc indexes must list every maintained doc, and must not list anything
 * absent. Checked bidirectionally: a one-way check passes on a stale entry.
 */
function checkIndexes() {
	const wanted = indexableDocs();

	const targets = [
		{ doc: "README.md", mode: "table", name: "README doc table" },
		{ doc: "AGENTS.md", mode: "map", name: "AGENTS.md Docs Map" },
	];

	for (const { doc, mode, name } of targets) {
		let entries = indexEntriesFrom(doc, mode);
		// AGENTS.md's map also carries glob-ish entries; keep only real paths.
		entries = entries.filter((e) => !e.includes("*"));

		const listed = new Set(
			entries.map((e) => normalize(e).replace(/\/$/, "")).filter((e) => e.endsWith(".md")),
		);

		// Directories listed in the index stand in for everything beneath them.
		const listedDirs = entries
			.map((e) => normalize(e).replace(/\/$/, ""))
			.filter((e) => !e.endsWith(".md"));

		const covered = (f) =>
			listed.has(f) || listedDirs.some((d) => d && f.startsWith(`${d}/`));

		for (const f of wanted) {
			if (!covered(f)) {
				report(1, "index-missing", doc, `${name} omits \`${f}\``);
			}
		}
		for (const e of listed) {
			if (!existsSync(join(ROOT, e))) {
				report(1, "index-dead", doc, `${name} lists \`${e}\`, which does not exist`);
			}
		}
	}
}

// --------------------------------------------------------------- tier 2

/**
 * Every truth item must be mentioned by every doc that is supposed to carry it.
 * `normalizers` maps a doc's spelling of an item to the truth spelling.
 */
function checkSurface(name, truth, targets, extraForms = () => [], strict = false, narrow = null) {
	for (const doc of targets) {
		if (!existsSync(join(ROOT, doc))) continue;
		const text = read(doc);
		// `narrow(text, truth)` lets a doc that points at the authoritative source
		// shrink the required set instead of skipping the check. Skipping outright
		// would make the check vacuous and blind to regressions; demanding full
		// enumeration anyway is what makes a doc list rot.
		const items = narrow ? narrow(text, truth) : truth;
		const tokens = new Set(backticked(doc).map((t) => t.trim()));
		for (const item of items) {
			const label = typeof item === "string" ? item : item.key;
			const forms = extraForms(item);
			// Strict mode accepts only a backticked key or a literal extra form.
			// Needed where the key is an ordinary English word: `extension` as a
			// CI job name word-matches "pi skill vs. extension" and hides a real
			// omission.
			const present = strict
				? tokens.has(label) || forms.some((f) => text.includes(f))
				: mentions(text, tokens, label, forms);
			if (!present) {
				report(2, name, doc, `\`${label}\` exists in source but is not documented here`);
			}
		}
	}
}

/**
 * CI jobs: enumerate-and-diff, not presence.
 *
 * A presence check cannot work here because the token is overloaded — README
 * says "The Pi companion extension (recall / store / reminders) has its own
 * config at ...", which is the pi client component, not the CI job of the same
 * display name. So instead: find the clause where a doc *enumerates* the jobs
 * and diff that set against ci.yml. A doc that never enumerates jobs is not
 * obliged to, and produces no finding.
 */
function checkCiJobList(docs) {
	const jobs = truthCiJobs();
	const accepted = new Map();
	for (const j of jobs) {
		accepted.set(j.key, j);
		accepted.set(j.name, j);
	}
	for (const doc of docs) {
		const text = read(doc);
		// "... jobs — `a`, `b`, `c` ..." / "... jobs: a, b, c ..." up to a sentence end.
		for (const m of text.matchAll(/\bjobs?\b([^.]*)/g)) {
			const clause = m[1];
			const named = [...clause.matchAll(/`([^`]+)`/g)].map((x) => x[1]);
			if (named.length < 2) continue; // not an enumeration
			const listed = named.filter((n) => accepted.has(n));
			if (listed.length < 2) continue;
			const seen = new Set(listed.map((n) => accepted.get(n).key));
			for (const j of jobs) {
				if (!seen.has(j.key)) {
					report(
						2,
						"ci-jobs",
						doc,
						`enumerates CI jobs but omits \`${j.key}\` (display name "${j.name}")`,
					);
				}
			}
			// The reverse check (a listed name that is not a job) is deliberately
			// absent: the clause boundary is a guess, so "...five jobs — `fmt`, ...
			// — on PRs to `main`" reports `main` as a phantom job. The omission
			// direction is the one that matters.
			break; // one enumeration per doc is enough
		}
	}
}
/**
 * axum 0.8 writes path params as `{id}`; the pre-0.8 syntax was `:id`. A doc
 * using the old spelling is not "missing" the route, but a reader who copies it
 * into a browser gets a 404, so it is reported separately as informational.
 */
function checkRouteSyntax(docs) {
	const routes = truthRoutes();
	for (const doc of docs) {
		const text = read(doc);
		for (const r of routes) {
			if (!r.includes("{")) continue;
			const legacy = r.replace(/\{(\w+)\}/g, ":$1");
			if (text.includes(legacy) && !text.includes(r)) {
				report(
					2,
					"route-syntax",
					doc,
					`\`${legacy}\` uses the pre-axum-0.8 path syntax; source has \`${r}\``,
					"info",
				);
			}
		}
	}
}

function tier2() {
	const tools = truthTools();
	// Skills spell tools with a per-client prefix; README/AGENTS use the bare name.
	checkSurface(
		"mcp-tools",
		tools,
		[
			"README.md",
			"AGENTS.md",
			"contrib/pi/skills/alexandria-memory/SKILL.md",
			"contrib/claude/skills/alexandria-memory/SKILL.md",
			"contrib/pi/extensions/alexandria/README.md",
		],
		(t) => [`alexandria_${t}`, `mcp__alexandria__${t}`],
	);
	checkPhantomTools(trackedDocs(), tools);

	checkSurface(
		"cli-subcommands",
		truthCli().map((c) => c.split(" ")[0]),
		["README.md"],
	);

	// A parameterised route is covered by its literal prefix: README documents
	// "/debug/memories" and describes the detail page in prose, which is a
	// legitimate way to cover `/debug/memories/{id}`.
	const literals = truthRoutes().filter((r) => !r.includes("{"));
	checkSurface("debug-routes", literals, ["README.md"]);
	checkRouteSyntax(trackedDocs());

	checkSurface("server-config-keys", truthServerConfigKeys(), [
		"docs/configuration.md",
	]);

	// Section-qualified forms count: `[recall] min_similarity` and `recall.min_similarity`
	// are both real spellings of the same key.
	const clientKeys = truthClientConfigKeys();
	checkSurface(
		"client-config-keys",
		clientKeys.map((k) => k.split(".").pop()),
		["docs/configuration.md", "contrib/pi/extensions/alexandria/README.md"],
		(item) => clientKeys.filter((k) => k.endsWith(`.${item}`)),
	);

	checkSurface("server-env-vars", truthServerEnvVars(), ["docs/configuration.md"]);
	// contrib/pi/README.md is deliberately not a target: it is an orientation
	// doc ("Which one do you want?") that points at the two authoritative ones.
	checkSurface("client-env-vars", truthClientEnvVars(), [
		"docs/configuration.md",
		"contrib/pi/extensions/alexandria/README.md",
	]);

	// A doc that says bare `just` lists every recipe still has to name `ci` and
	// everything `ci` depends on, plus `ext-install` — the one-time prerequisite
	// without which `ext-test` fails on a fresh checkout, and which `ci` does not
	// list because it assumes the install already happened.
	const bearing = new Set([...loadBearingRecipes(), "ext-install"]);
	const listsAllRecipes = /bare `just` lists|`just`\s+# list every recipe/i;
	checkSurface(
		"just-recipes",
		truthJustRecipes(),
		["README.md", "AGENTS.md"],
		() => [],
		false,
		(text, truth) => (listsAllRecipes.test(text) ? truth.filter((r) => bearing.has(r)) : truth),
	);

	checkCiJobList(trackedDocs());
}

// ------------------------------------------------- tier 3: claim inventory

/**
 * Split a doc into claim units: a bullet, a numbered-list item, or a paragraph.
 * Sentences are NOT the unit — a bullet in AGENTS.md is often one multi-clause
 * assertion, and fragmenting it produces rows no reviewer can verify standalone.
 *
 * A unit is kept when it carries a checkable assertion: a backticked symbol, a
 * number, or a modal. Purely navigational prose is dropped and counted, so
 * "did you check everything" is answerable.
 */
const ASSERTION_RE =
	/`[^`]+`|\b\d+\b|\b(must|never|only|always|exactly|cannot|refuses|does not|do not|is not|no |every|all)\b/i;

function extractClaims(docs) {
	const rows = [];
	let skipped = 0;
	for (const doc of docs) {
		const lines = read(doc).split("\n");
		let inFence = false;
		let unit = null;
		const flush = () => {
			if (!unit) return;
			const { lines: buf, from, to } = unit;
			unit = null;
			const text = buf.join("\n").trim();
			if (!text) return;
			if (/^#{1,6}\s/.test(text) && buf.length === 1) {
				skipped++;
				return;
			}
			if (!ASSERTION_RE.test(text)) {
				skipped++;
				return;
			}
			rows.push({
				id: `C${String(rows.length + 1).padStart(4, "0")}`,
				doc,
				lines: `${from}-${to}`,
				claim: text,
			});
		};
		lines.forEach((line, i) => {
			const n = i + 1;
			if (/^\s*(```|~~~)/.test(line)) {
				inFence = !inFence;
				return;
			}
			if (inFence) return;

			const isBullet = /^\s*(?:[-*+]|\d+\.)\s+/.test(line);
			const isBlank = line.trim() === "";

			if (isBlank) {
				flush();
				return;
			}
			// A new top-level bullet starts a new unit; a continuation line joins.
			if (isBullet && !/^\s{2,}\S/.test(line) && unit && /^\s*(?:[-*+]|\d+\.)\s+/.test(unit.lines[0])) {
				flush();
			}			if (!unit) unit = { lines: [], from: n, to: n };
			unit.lines.push(line);
			unit.to = n;
		});
		flush();
	}
	return { rows, skipped };
}

// ------------------------------------------------------------------- main

const args = process.argv.slice(2);
const asJson = args.includes("--json");
const claimsMode = args.includes("--claims");
const tierArg = args.find((a) => a.startsWith("--tier"));
const onlyTier = tierArg ? Number(tierArg.split("=")[1] ?? args[args.indexOf(tierArg) + 1]) : null;

const docs = trackedDocs();

if (claimsMode) {
	const { rows, skipped } = extractClaims(docs);
	const byDoc = {};
	for (const r of rows) byDoc[r.doc] = (byDoc[r.doc] ?? 0) + 1;
	process.stdout.write(
		JSON.stringify({ total: rows.length, skipped, byDoc, rows }, null, 2) + "\n",
	);
	process.exit(0);
}

if (onlyTier !== 1) {
	checkLinks(docs);
	checkAnchors(docs);
	checkBacktickedPaths(docs);
	checkIndexes();
}
if (onlyTier !== 2) tier2();

if (asJson) {
	process.stdout.write(JSON.stringify({ count: findings.length, findings }, null, 2) + "\n");
} else {
	const groups = new Map();
	for (const f of findings) {
		const k = `T${f.tier} ${f.check}${f.severity === "info" ? " (info)" : ""}`;
		if (!groups.has(k)) groups.set(k, []);
		groups.get(k).push(f);
	}
	const blocking = findings.filter((f) => f.severity !== "info").length;
	const advisory = findings.length - blocking;
	console.log(
		`docs-audit: ${blocking} finding(s), ${advisory} advisory, across ${docs.length} docs\n`,
	);
	for (const [k, list] of [...groups.entries()].sort()) {
		console.log(`── ${k} (${list.length})`);
		for (const f of list) console.log(`   ${f.doc}: ${f.detail}`);
		console.log();
	}
}

// Advisory findings are reported but do not fail the run, so the script can be
// gated later without first having to adjudicate every record doc's history.
process.exit(findings.some((f) => f.severity !== "info") ? 1 : 0);
