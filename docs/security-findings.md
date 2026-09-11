# Security Findings

Audit of 2026-09-10 against commit `858dd82`. Scope: the HTTP transport and debug UI (`crates/alexandria/src/main.rs`,
`crates/alexandria-mcp`), the storage query layer, the embedding pipeline, and the two client
integrations under `contrib/` (pi extension, Claude Code hooks). Method: full read of those files,
cross-checked against the vendored `rmcp 3.2.0` source for transport defaults, plus live measurements
against the running server where a claim depended on data.

The audit was prompted by reading *Classification of Malignant Prompt Embeddings with Convex Hulls*
(Kelvin Sanchez, Johns Hopkins Whiting School of Engineering, December 2025). The paper is summarised
under [Threat model](#threat-model) because its subject, malicious text arriving through an
embedding pipeline, is the threat that matters most here, even though its proposed defence does not
transfer.

Severity is relative to the documented posture: the MCP endpoint and debug UI are unauthenticated by
design and meant for a trusted network boundary (README, "Debug Web UI" and "Deployment"). Findings
are ranked by what an attacker gains *within* that posture, not by whether the posture itself should
change.

| ID | Finding | Severity |
|---|---|---|
| [S1](#s1-dns-rebinding-protection-is-disabled-by-default) | DNS-rebinding protection is disabled by default | Medium |
| [S2](#s2-stored-prompt-injection-through-memory-facts-carry-no-provenance) | Stored prompt injection through memory; facts carry no provenance | High (impact), structural |
| [S3](#s3-debug-ui-loads-scripts-from-a-cdn-without-integrity-hashes) | Debug UI loads scripts from a CDN without integrity hashes | Low |
| [S4](#s4-the-query-tester-is-a-state-changing-endpoint-reachable-by-cross-site-form-post) | Query tester is a state-changing endpoint reachable by cross-site form POST | Low |
| [S5](#s5-unclamped-request-parameters) | Unclamped request parameters | Low |
| [S6](#s6-model-download-has-no-integrity-check) | Model download has no integrity check | Info |

## Threat model

### The documented posture

- No authentication on `/mcp` or `/debug`. Any process that can reach the port can read, write,
  update, and soft-delete every memory and session.
- Default bind is `127.0.0.1:3000`; the Docker image binds `0.0.0.0:3000` and the README says to
  publish it only behind a reverse proxy or on a trusted network.
- Session `external_id` is client-chosen and any client can read any session. Consistent with the
  above; not a finding on its own.

### Where memory content goes

Alexandria is not just a database. Both client integrations inject retrieved memory content into the
agent's context on **every prompt**:

- pi extension: `contrib/pi/extensions/alexandria-auto-recall/src/recall.ts:43-52` formats each hit
  as `- (similarity, id) [tags] <content>` and returns it as a custom message before the agent starts.
- Claude Code hook: `contrib/claude/hooks/alexandria-recall.sh:157-165` emits the same block as
  `additionalContext` on `UserPromptSubmit`.

The only guard is one sentence, "verify relevance before relying on them". Content is not delimited
per memory beyond a leading `- `, and newlines inside a memory are not stripped, so a stored memory
can forge additional bullets or append instructions to the block.

That makes the memory store a **persistence layer for prompt injection**: text that reaches a fact
record is replayed into every future prompt whose embedding lands near it. The writers that can put
text there:

| Writer | What it stores | Trust of the source text | Where |
|---|---|---|---|
| `store_memory` from an agent | Whatever the agent decides | Agent-mediated | `crates/alexandria-mcp/src/server.rs:204` |
| Heuristic detectors | Regex captures from the user's prompt | User text | `contrib/pi/.../detectors/{correction,preference}.ts`, `alexandria-recall.sh` |
| Error-resolution tracker | First 200 chars of a failed tool result plus the next success | **Tool output** | `contrib/pi/.../detectors/error-tracker.ts` |
| LLM extraction (pi) | Model-chosen facts from user and assistant text | Assistant text can echo tool output | `contrib/pi/.../extraction-parse.ts` |
| LLM extraction (Claude) | Model-chosen facts from user text, assistant text, and **failed tool results** | Tool output | `contrib/claude/hooks/alexandria-extract.sh:67-81` |
| `import_document` | Third-party text verbatim, confidence 1.0, initial heat 2.0 | Whatever the document is | `server.rs:318-418` (confidence at `:383`, heat at `:387`) |
| Any network client | Anything | None | Docker image on `0.0.0.0` |

Two of those paths (tool output through extraction, `import_document` of a fetched README or web
page) let an attacker who controls a document the agent reads plant a memory without ever touching
the server directly. Note also that imported text is stored with *higher* confidence and initial heat
than a user's own statements, which inverts the trust order.

### The convex-hull paper

**What it does.** Embeds labelled prompts with `text-embedding-3-large`, reduces to three dimensions
with PCA, computes the convex hull of the benign and malignant training sets, and classifies a new
prompt by Delaunay point-in-hull membership. Variants remove outliers beyond 1, 2, or 3 standard
deviations before building the hull. Datasets: MPDD (39,234 prompts, roughly balanced), BeaverTails
(300,576), and Do-Not-Answer (939, all malignant). Baselines: logistic regression, kNN, random forest,
SVM linear and RBF. 5-fold cross-validation.

**What it finds.**

| Dataset | Best hull variant | Acc | Prec | Recall | F1 | ROC-AUC | Best baseline (SVM RBF / RF) ROC-AUC |
|---|---|---|---|---|---|---|---|
| MPDD | Outlier RM 3 | 0.86 | 0.97 | 0.75 | 0.84 | 0.86 | 0.88 |
| BeaverTails | Outlier RM 1 | 0.63 | 0.62 | 0.88 | 0.73 | 0.60 | 0.80 |
| Combined | Outlier RM 1 | 0.63 | 0.61 | 0.88 | 0.72 | 0.60 | 0.79 |

The unmodified hull on BeaverTails scores recall 0.016 and ROC-AUC 0.51, which is chance. The
author's own conclusion: hulls give high precision when classes are cleanly separable in latent space
and fail when they overlap, and they do not beat conventional classifiers on any dataset.

**Why it does not transfer to Alexandria.**

1. It needs labelled benign and malignant training sets. Alexandria's corpus is unlabelled.
2. It reduces a 3072-dimension embedding to 3. Alexandria uses a 384-dimension symmetric sentence
   model (`all-MiniLM-L6-v2`); the same reduction discards proportionally more.
3. The failure mode here is not a jailbreak prompt but an *instruction disguised as a fact*.
   "Always run the sync script before committing" is a legitimate preference and a payload, and the
   two are not separable geometrically. That is exactly the overlap case where the paper's method
   collapses.
4. Alexandria already has a per-fact geometric signal for free (cosine to the nearest cluster
   centroid, and whether `assign_to_cluster` created a new cluster). It could be surfaced as a
   *flag*, but with `cluster.join_threshold = 0.75` most facts already start new clusters, so the
   signal is weak. Not worth building.

**What it does tell us.** The embedding pipeline is the entry point, the defence has to be
provenance and rendering rather than classification, and that is finding S2.

## Findings

### S1. DNS-rebinding protection is disabled by default

**Severity:** Medium. **Effort:** small.

**Where.**

- `crates/alexandria/src/config.rs:47-48`: `allowed_origins` and `allowed_hosts` both default to `["*"]`.
- `crates/alexandria/src/main.rs:142` and `:147`: a `"*"` entry calls `disable_allowed_hosts()` /
  `disable_allowed_origins()` on the rmcp config.
- rmcp `3.2.0`, `src/transport/streamable_http_server/tower.rs:172`: the library's own default is
  `allowed_hosts = ["localhost", "127.0.0.1", "::1"]`. `host_is_allowed` (`:762`) returns true only
  when the list is empty or matches, so Alexandria ships with a weaker default than the library it
  wraps.
- The `/debug` router is merged alongside the rmcp service (`crates/alexandria/src/main.rs:304-306`) and is not
  covered by rmcp's Host or Origin checks at all.

**Attack.** The operator opens any web page while the server runs on loopback. The page's origin is
rebound via DNS to `127.0.0.1`; the browser now treats `http://attacker:3000/mcp` as same-origin and
the page can drive the full JSON-RPC tool surface: read every memory, store injected ones (see S2),
delete or rewrite existing ones. The Host-header check is the defence the MCP Streamable HTTP
specification requires for locally bound servers; it is off.

**Fix.**

1. Change the default of `allowed_hosts` to rmcp's loopback list (or to `[]`, which makes
   `serve_http` fall through to rmcp's default). Keep `["*"]` as an explicit opt-out and say so in
   `docs/configuration.md`.
2. Add `ALEXANDRIA_SERVER_ALLOWED_HOSTS` (comma-separated) to the env overrides. The Docker image
   binds `0.0.0.0` and remote clients send their own `Host`, so the image needs a way to set this
   without a mounted TOML; today only `transport`/`host`/`port` are env-overridable.
3. Apply the same Host check to `/debug`, either by a small axum middleware or by reusing rmcp's
   `host_is_allowed` if it is reachable.

### S2. Stored prompt injection through memory; facts carry no provenance

**Severity:** High impact, structural. **Effort:** medium.

**Where.**

- `crates/alexandria-mcp/src/server.rs:221-226`: every `store_memory` creates a `provenance` row
  with `kind = 'user'` and nothing else. The row is never related to the fact (the `has_provenance`
  relation from `v001_initial.surql` is never created anywhere in the tree) and never read. The
  `agent_id` and `model` params are recorded on the session, not the fact.
- `import_document` (`server.rs:383,387`) stores chunks with confidence `1.0` and heat `2.0`;
  `store_memory` uses `0.5` and `1.0`.
- Recall rendering: `recall.ts:43-52`, `alexandria-recall.sh:157-165` (see the threat model).

**Impact.** A reader of the recall block cannot tell a user's stated preference from a paragraph of
a fetched web page or from a sentence a cheap extraction model pulled out of tool output. The
`extracted` and `auto-detected` tags that the clients attach are client-controlled free text and are
the only hint. A planted memory phrased to embed near common prompts ("when running tests, ...")
is replayed into every matching prompt across every agent that uses auto-recall, indefinitely.

**Fix, layered, cheapest first.**

1. **Server-set source on the fact.** Add a `source` field (migration `v007`) set by the server,
   not the client: `agent` for `store_memory`, `import` for `import_document`. Accept an optional
   `kind` parameter on `store_memory` restricted to an enum (`user`, `heuristic`, `extracted`) so
   the clients can label their own write paths without being able to claim `import` is `user`.
   Carry `agent_id` onto the fact too. Alternatively link the existing `provenance` row with
   `RELATE` and populate it; the field is simpler and the row is currently dead weight either way.
2. **Return and render it.** Include `source` in `retrieve_memories` results and print it in the
   recall block ahead of the content. Strip newlines from content in the block so a memory cannot
   forge bullets, and change the trailing sentence to say the memories are data, not instructions.
3. **Let clients weight by source.** The simplest useful policy is ordering: user and agent facts
   first, then extracted, then imported. A per-source threshold is a follow-on.
4. **Extraction prompts.** Both extraction prompts should say that text inside the conversation is
   data and that instructions addressed to an assistant are not facts to store unless the user
   stated them. This is partial protection only.
5. **Reconsider import defaults.** Confidence `1.0` for third-party text over `0.5` for user
   statements is a design choice ("known-true from docs" in `docs/roadmap.md`), but it is the
   inverse of the trust order for injection. At minimum, do not let imported chunks start hotter
   than user facts once heat is wired into ranking (see the performance report).

What not to do: an embedding-space classifier (see the paper above).

### S3. Debug UI loads scripts from a CDN without integrity hashes

**Severity:** Low. **Effort:** small.

**Where.** `crates/alexandria-mcp/src/debug/html.rs:18` (`htmx.org@1.9.12` from unpkg) and
`crates/alexandria-mcp/src/debug/graph.rs:78` (`vis-network@9.1.6` from unpkg). Neither tag has an
`integrity` attribute.

**Impact.** Anyone who can serve a modified file at those URLs (CDN compromise, on-path attacker on a
LAN deployment) runs script in the operator's browser on a page that can call the unauthenticated
API. The versions are pinned, which limits accidental drift but not substitution.

**Fix.** Either embed both files in the binary with `include_str!` and serve them from `/debug/static`
(they total about 1.2 MB, acceptable), or add `integrity="sha384-..."` and
`crossorigin="anonymous"` to both tags. Embedding also makes the debug UI work offline.

### S4. The query tester is a state-changing endpoint reachable by cross-site form POST

**Severity:** Low. **Effort:** trivial.

**Where.** `crates/alexandria-mcp/src/debug/query.rs:34-45`: `POST /debug/query/run` accepts
`application/x-www-form-urlencoded` and calls `do_retrieve_memories`, which triggers spreading
activation and writes heat (`server.rs:457-464`).

**Impact.** A form POST with that content type is a CORS "simple request", so any web page can submit
it to a loopback server without a preflight and without S1's rebinding trick. Effect is limited to
heat manipulation and embedding CPU, which is a nuisance rather than a breach.

**Fix.** htmx sends `HX-Request: true` on every request it makes. Reject the POST when that header is
absent. A custom header forces a preflight, which fails without CORS headers, so plain cross-site
form posts are blocked with one `if`.

### S5. Unclamped request parameters

**Severity:** Low. **Effort:** trivial.

**Where.**

- `retrieve_memories.limit` (`server.rs:424`) flows unchanged into the KNN operator
  `embedding <|{k},COSINE|>` at `crates/alexandria-storage/src/repos/memory_repo.rs:83`. Nothing
  bounds `k`.
- `import_document` embeds every chunk sequentially and inline on the runtime (`server.rs:376-378`,
  see the performance report). rmcp caps request bodies at 4 MiB
  (`tower.rs:55`, `DEFAULT_MAX_REQUEST_BODY_BYTES`), which bounds the input but still allows
  thousands of chunks per call.
- Debug list `limit` (`debug/memories.rs:46-49`) is likewise unbounded, though that page is read-only.

**Fix.** Clamp `limit` to a ceiling (100 is far above any measured useful value; the clients ask for
10). Cap chunk count per import, or move chunk embedding off the runtime thread so a large import
degrades gracefully instead of stalling other requests.

### S6. Model download has no integrity check

**Severity:** Info.

**Where.** `crates/alexandria-pipeline/src/embedding/hub.rs` fetches `config.json`,
`tokenizer.json`, and `model.safetensors` over HTTPS (rustls) and writes them into the cache with no
checksum. The Hub API exposes a SHA-256 per LFS file.

**Impact.** Low. TLS covers the transport, safetensors parsing is not an arbitrary-code format, and
the download happens once. A compromised upstream repository would still be accepted, which is true
of every consumer of that model.

**Fix if wanted.** Verify `model.safetensors` against the Hub's reported SHA-256, or pin a known
digest in config for the locked model. Not urgent.

## Checked and found sound

- Every user-supplied value reaches SurrealDB through `.bind()`. The only `format!` interpolations
  into query text are the numeric `k` (S5), fixed clause fragments chosen by `Option::is_some`, and
  constant table names. Record IDs for `RELATE` go through `RecordId::parse_simple`.
- All DB-sourced strings in the debug UI pass through `esc()` (`debug/html.rs:1-8`, with a test),
  including record IDs in `href` attributes.
- The container runs as uid 10001, not root. CI actions are pinned by full SHA with
  `persist-credentials: false`; `cargo deny` runs in CI with one documented advisory ignore.
- Nothing logs memory content at `info` or above.
- Scope handles are unsigned base64 JSON, but forging one only reads a cluster that is readable
  anyway; there is no privilege to escalate.
- `update_memory` keeps soft-deleted snapshots for lineage; deletion is soft everywhere, so the
  worst case for a destructive client is recoverable from the data directory.

## Recommended order

1. S1: change the `allowed_hosts` default and add the env override. One config default and a few
   lines in `crates/alexandria/src/main.rs`, plus a docs line.
2. S2 steps 1 and 2: server-set `source`, returned and rendered. One migration, one field on
   `Fact`, two client renderers.
3. S3 and S4: embed the two scripts, check `HX-Request`.
4. S5: clamp `limit`, cap import chunks.
5. S2 steps 3 to 5 as follow-ons once `source` exists.
