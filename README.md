# Alexandria

Agent memory server with tiered maturity, hierarchical clustering, spreading activation, and progressive recall. Runs as a persistent [MCP](https://modelcontextprotocol.io/) service backed by embedded [SurrealDB](https://surrealdb.com/).

## Features

- **Semantic search** — Cosine similarity over local embeddings (default MiniLM via Candle; opt-in release-gated Granite via Burn, both native Rust)
- **Ebbinghaus heat model** — Memories have heat (recency) and stability (spaced repetition). Frequently accessed memories stay hot; forgotten ones cool. Heat is recorded on retrieval but is not yet a ranking input — retrieval still ranks by cosine similarity (see [`A2`](docs/performance-and-ability-findings.md#a2-the-heat-model-is-inert)).
- **Spreading activation** — Accessing a memory warms its graph neighbors. Heat propagates along edges with configurable decay.
- **Graph edges** — Memories link via `derived_from` and `extracted_from` edges. `relates_to`, `supports` and `contradicts` exist in the schema but nothing in production writes them yet; auto-linking them is planned v0.3 work
- **Hierarchical clustering** — Automatic cluster assignment on store, plus a background housekeeping scheduler that splits, merges, collapses byte-identical duplicates and lowers the confidence of memories that have gone cold without ever being retrieved, all with a queryable audit log that one `alexandria dream` pass can be reviewed (or run on demand) against. Legacy split/merge currently skips owned or mixed populations pending scoped maintenance ([details](docs/configuration.md#dreaming)).
- **Progressive recall** — Two-phase retrieval: broad cluster matching first, then scope-narrowing within a cluster
- **Session memory** — Group memories by conversation, search within a session, and close it out with a summary
- **Document import** — Chunk by heading, paragraph, or fixed size with batch tracking and `extracted_from` lineage
- **Reminders** — One-shot and recurring follow-ups delivered on the next interaction, with project targeting and overdue escalation
- **Persistent storage** — SurrealKV on disk, survives restarts
- **Debug web UI** — Browser for memories, clusters, sessions, graph neighborhoods, reminder migration holds, cluster maintenance history, and a live query tester. Its two disclosed writes are retrieval heat activation and owner-authorized reminder mapping (see [Debug Web UI](#debug-web-ui))
- **Schema migrations** — Versioned `.surql` files with forward-only migration runner

## Quick Start

```bash
# Build and install
cargo install --path crates/alexandria

# Create config
mkdir -p ~/.config/alexandria
cat > ~/.config/alexandria/config.toml << 'EOF'
[server]
transport = "http"
port = 3000

[embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"
device = "cpu"
# Tokens of each memory the search sees. 256 is the default and what has been measured;
# up to 512 is experimental. A database created before this key existed is stamped 128 and
# needs `alexandria migrate-embeddings` before it boots at the default. Locked on first run
# and only ever raised.
max_tokens = 256
EOF

# Run
alexandria
```

First run with the default MiniLM configuration downloads the embedding model from HuggingFace Hub (~80MB).

### Native Granite (opt-in, release-gated)

The exact model ID `ibm-granite/granite-embedding-311m-multilingual-r2` selects native Rust Burn inference: 768D FP32, CLS pooling, and a default/cap of 8192 tokens. CPU is the default; explicit `rocm[:index]` or `vulkan[:index]` needs the corresponding binary Cargo feature (`granite-rocm` or `granite-vulkan`). A missing backend/device fails startup rather than falling back. Serving uses exact input lengths and a bounded serial worker.

Build the requested backend explicitly; changing `embedding.device` cannot add it to an existing binary:

```sh
cargo build --release --locked -p alexandria                         # CPU
cargo build --release --locked -p alexandria --features granite-rocm # ROCm
cargo build --release --locked -p alexandria --features granite-vulkan # Vulkan
```

GPU driver/toolchain availability is a separate requirement; the shipped Alpine image builds
CPU-only and contains neither GPU runtime nor model artifacts. For container CPU opt-in, mount
the exact verified local artifact directory read-only and set `artifact_dir` to its in-container
path; ensure the service UID can read it. Granite's XDG artifact cache is separate from `HF_HOME`
(the MiniLM cache); persist it explicitly if using it. No driver, model or cache is supplied by
changing a device selector.

MiniLM remains the installation default. The checksum-pinned Granite pack is **not published yet**: opt-in startup requires verified local artifacts or an already verified cache. A local directory supplies bytes, not replacement checksums. Publication is gated by tokenizer licensing review as well as release approval; the Gemma3-derived tokenizer is not covered by the weights' Apache-2.0 license alone.

Existing databases cannot be relabelled Granite. Stop the source server, configure the target model/budget while keeping the source data path, then use `alexandria migrate-embeddings --output-data-dir /path/to/new-target`. `--resume` continues an owned target or reports a completed one without loading weights. The original source is retained; model, budget and data path cutover (and paired rollback) remain manual. The legacy in-place command refuses Granite and owned migration targets. Do not change live model/path settings or reuse MiniLM cutoff calibration as proof of Granite quality. See [embedding configuration](docs/configuration.md#embedding) and the [artifact curator](tools/granite-artifact/README.md).

Granite's cosine scale differs from MiniLM's. In the small public synthetic pilot, retaining a MiniLM-style floor returned results for every unanswerable query; higher floors traded those errors for lost valid passages. This is not sufficient evidence to change defaults. See [Granite test data and release gates](docs/granite-test-data.md) for the multi-relevant pilot, native evaluation procedure and remaining calibration requirements.

## Command Line

`alexandria` with no arguments starts the server. The three subcommands are maintenance tools
that run once and exit.

| Command | What it does |
| --- | --- |
| `alexandria` | Start the server on the configured transport. |
| `alexandria migrate-embeddings [--force]` | Legacy-only: re-embed the whole corpus with the model and `max_tokens` in `config.toml`. Granite requires a separate offline target and is refused by this command. Needed after a deliberate model change or after raising `max_tokens`; `--force` re-embeds even when nothing changed — see [docs/configuration.md](docs/configuration.md). |
| `alexandria migrate-embeddings --output-data-dir PATH [--resume] [--force]` | Copy a stopped persistent source into a separate, nonoverlapping target, then rewrite and verify it. Logs go to stderr and the completed JSON report to stdout. Resume requires the original adjacent receipt and matching journal/configuration; `--force` does not bypass ownership or replay a completed target. No automatic cutover, deletion, swap or restart. |
| `alexandria bench-retrieval` | Measure how well the configured model separates a correct answer from the rest of the corpus, and print the score distributions and the limit × threshold grid that the `retrieve.min_similarity` floor and the client recall defaults are judged against — see [docs/minilm-test-data.md](docs/minilm-test-data.md). |
| `alexandria dream [--job NAME]... [--max-rows N]` | Run one or more of the five housekeeping jobs **once, now**, and print what each did (`job=… examined=… acted=… skipped=…`) plus the run id its audit rows carry, so `/debug/maintenance?run=<id>` shows exactly what that pass changed. With no `--job` it runs all five. The way to answer "why did my corpus change" or "run the collapse now" without editing `[dreaming]` cadences and restarting the service — see [docs/configuration.md](docs/configuration.md). |
| `alexandria refinement --help` | Inspect routes, grant or revoke disclosed destination consent, or set an approval preference for the configured trusted-local account. See the [refinement configuration](docs/configuration.md#refinement). |
| `alexandria dream --job refine --workspace ID --scope personal` | Run one explicit offline refinement turn for the trusted-local administrator's own workspace with current consent. `--scope project:ID` selects one project instead. Results remain pending for review; this is not a sixth housekeeping job. |
| `alexandria --help` | Print the command summary. |

The legacy in-place command, benchmark and `dream` open the data dir directly, and SurrealKV
is single-writer, so the server has to be stopped first. `alexandria dream` refuses a held
store before any job runs; `dream --help` states the constraint too. The separate-target
migration instead locks the existing source `LOCK` read-only and never opens its storage
engine; it opens only the copied target. Initial source-lock interoperability is admitted on
Linux only. Keep the adjacent source-manifest receipt with the target, especially while it is
unfinished. `bench-retrieval` can instead run against a copy of the data dir via
`ALEXANDRIA_DATA_DIR`, which keeps the server down only for a `cp`. It is not read-only: like
server boot it checks the embedding-model lock (refusing on a mismatch) and defines the HNSW
index if it is missing. The five housekeeping jobs need neither the model nor the index — they read the
stored vectors, heat and confidence. It validates stored embedding readiness/identity before
schema or job writes and refuses unfinished or corrupt migration targets, without loading a
provider or requiring the configured model to match. The cap is one allowance per capped job per pass **across all eligible partitions** — the dispatcher enumerates active accounts' partitions, rotates fairly with a persisted cursor, and shares the allowance, so one account's many projects cannot take every turn and a suspended account's workspace is neither enumerated nor written. `cluster` and `merge` are not partitioned yet: they remain legacy-gated whole-store jobs (they refuse owned/workspace data by construction), with partitioned owned-cluster maintenance as named follow-up work. `--max-rows N` overrides `dreaming.max_rows_per_run`
for that invocation only, without touching the config file.

## MCP Tools

The server exposes **17 MCP tools** (13 memory/session/reminder tools plus four project/classification tools). This table is the current server surface; the companion client additions do not add tools.

| Tool | Description |
| ------ | ------------- |
| `store_memory` | Store text with auto-embedding, clustering, and heat initialization; optional `session_id` to group it, plus `scope` (`personal`/`project`) with `project`/`context_project` to choose where it lands — a hosted request that names neither is refused before anything is embedded. Returns `truncated: true` when the text is longer than `embedding.max_tokens`, meaning only its head is searchable, and `scope` to confirm where the row went |
| `retrieve_memories` | Semantic similarity search with spreading activation on top results; optional `session_id` to narrow within the scope, plus `scope` (`auto`/`personal`/`project`/`assigned`) and `project` to choose which partitions are searched. The response reports the selection that actually ran as `scope` — a label, never a partition handle |
| `recall` | Progressive two-phase recall: broad cluster scan → focused scope narrowing; takes the same `scope`/`project` selection as `retrieve_memories` and applies the same population predicate to cluster members, reporting the selection it ran as `scope` |
| `update_memory` | Update content/tags/confidence; content changes re-embed and create lineage |
| `import_document` | Import and chunk documents with `extracted_from` edge tracking; takes the same `scope`/`project`/`context_project` target, refused before the batch is embedded when a hosted request names none |
| `delete_memory` | Soft-delete a memory by ID |
| `get_session` | Return a session's metadata plus the memories stored during it, inside the population selected by `project`; `view: "archive"` is the deliberate whole-session read that also returns the account-private summary and tags |
| `list_sessions` | List sessions newest-first with live memory counts restricted to the population selected by `project`; filter by `agent_id` or `finalized`, and by `tag` only in `view: "archive"` |
| `finalize_session` | Close a session with a summary, tags, and `ended_at` |
| `set_reminder` | Schedule a one-shot (`due_at`) or recurring (named pattern / cron) reminder; response previews the next fire times |
| `check_reminders` | Deliver due reminders and mark them consumed (global, project-matching, or escalated) |
| `list_reminders` | List scheduled reminders by status and target project, oldest-due first, paginated |
| `cancel_reminder` | Soft-cancel a reminder by ID |
| `resolve_project` | Establish which project the current work belongs to from a sanitized origin remote or a project name/ID, and return the opaque project ID to use as `context_project`; the only tool that can create a project, so no agent guesses a partition into existence |
| `list_projects` | List the projects you own with live memory counts, paged by opaque cursor (default 20, max 100); never names another workspace's project, not even by existence |
| `list_unassigned_memories` | Enumerate the Unassigned partition for owner review, paged by cursor; the response says plainly that these rows are review state and are in no recall path, which is the point of the partition |
| `propose_memory_scope` | Propose where a memory belongs, naming the revision it was read at (1–20 per call). Records a pending proposal and moves nothing: only the owner's decision, made later, changes a memory's scope |

Tool and parameter descriptions are written directively ("call this proactively when…") because
that measurably changes how often client LLMs reach for them unprompted.

### Session memory

Passing `session_id` to `store_memory` groups memories under a caller-chosen handle, and the session
is created on first use. `retrieve_memories` with `session_id` restricts ranking to that session's
memories, `get_session` reads the whole session back, `list_sessions` finds a session whose id you
don't have, and `finalize_session` records its summary.
See [docs/session-memory.md](docs/session-memory.md) for the data model and current limitations.

### Reminders

Reminders are scheduled messages — "remind me", "check the deploy at 3pm" — delivered into the
agent's context on the next interaction after they come due. There is no background timer: due-ness is
evaluated at query time, so delivery is lazy, transport-agnostic, and survives server downtime. The
cost of that design is that nothing reaches anyone until a client asks: the pi companion calls
`check_reminders` once per prompt and additionally notifies the human, and that client-side step is
where "and the user" actually lives — a client that never calls the tool receives nothing.
`set_reminder` validates schedules at set time and echoes the parsed next fire times for confirmation;
`check_reminders` delivers and consumes them, one-way. All reminder operations are account-bound.
Omit `set_reminder.target_project` for a global reminder **within your account**, or supply an existing
owned `project:<opaque-id>` handle. `check_reminders.project` and `list_reminders.target_project`
accept that same owned handle, not a name, alias, or checkout basename. Project tool results use
canonical SurrealDB record text: copy it verbatim, including any quoting/escaping that distinguishes
numeric, string, or literal-backtick keys. Do not strip delimiters or rebuild it from a display label.
Unknown or foreign handles
are refused even when nothing is due; omit a check context when no handle is known.

A valid project target overdue by at least `[reminders].escalation_hours` escalates within its owner's
account. A historical unresolved text target or `migration_hold` row stays pending and suppressed,
including from the read-only due sample; escalation never turns it into a global reminder. No automatic
legacy-name mapping is performed. Unfiltered `list_reminders` still exposes owned held/unresolved
history for inspection (`target: null` for unresolved text, with separate legacy fields). The owner can
review held rows, their original target text and an exact outstanding count at `/debug/reminder-mapping`,
then release each with a revision-checked choice of Personal or an owned project. The journal records the
mapping while preserving the legacy text and schedule/delivery history.
Overdue eligible items are surfaced read-only, capped and oldest-due first, in the `due_reminders`
array of `retrieve_memories` and `recall` responses. Targets there and in delivery responses are
`global` or a single typed `project:<opaque-id>` handle, never a doubled prefix.

Compatibility: older companion clients that send repository basenames do not satisfy this server
contract. Configure an existing owned project handle where supported, or omit the context; automatic
client-side handle discovery and historical target mapping are separate work, not provided here.

Recurring schedules are evaluated as wall clock in `[reminders].timezone`, so a daily 09:00 stays 09:00
local across a DST change: a time the spring-forward gap removed does not fire that day, and a time a
fall-back fold repeats fires once, on its first pass. The cron escape hatch carries two dialect
surprises — numeric days of week are 1=Sunday (write day names), and a restricted day-of-month ANDs
with a restricted day-of-week instead of ORing as Vixie cron does — so `set_reminder` puts a `warning`
in the response when it recognizes the second one.

### Getting agents to actually use memory

A memory server is only useful if agents reach for it unprompted. Alexandria nudges this at
three levels:

1. **MCP `instructions`** — the server advertises usage guidance (when to read vs. write memory
   and how reminders get delivered) in its `initialize` response via `ServerInfo.instructions`.
   Any MCP-compliant client can surface this to the model. Tool descriptions are also written
   directively ("call this proactively whenever...") rather than just describing mechanics.
2. **Client-side skill** — [`contrib/pi/skills/alexandria-memory/`](contrib/pi/skills/alexandria-memory/)
   (Pi) and [`contrib/claude/skills/alexandria-memory/`](contrib/claude/skills/alexandria-memory/)
   (Claude Code) document concrete trigger conditions and tool choice guidance, mirroring how other high-usage
   MCP tools ship skills alongside themselves.
3. **Optional companion extension** — [`contrib/pi/extensions/alexandria/`](contrib/pi/extensions/alexandria/)
   performs prompt-time recall and reminder checks, but has no automatic write side. It resolves
   project context through a sanitized origin remote (or `ALEXANDRIA_PROJECT_ID`) and requires its
   independent `ALEXANDRIA_API_TOKEN`.
   It does not automatically store observed content or extract a session transcript: agents must call
   `store_memory` or `import_document` explicitly when something should be retained. The Claude Code
   equivalent is the recall/session hooks at [`contrib/claude/hooks/`](contrib/claude/hooks/); Claude's
   hook is Personal-only unless `ALEXANDRIA_PROJECT_ID` is set, and it likewise has no automatic
   observation writer.

Items 2 and 3 are client-side integrations, not part of the MCP server itself — see
[`contrib/pi/README.md`](contrib/pi/README.md) and [`contrib/claude/README.md`](contrib/claude/README.md)
for what they are and how to install them.

## Debug Web UI

When `transport = "http"`, a debug web UI is served alongside the MCP endpoint at
`http://<host>:<port>/debug`. It is **authenticated**: every page and every JSON endpoint belongs to the
signed-in account, and shows only that account's own memories, sessions, clusters, graph, reminders and
query results. It has two disclosed write paths: the Query Tester's non-dry `retrieve` run performs
spreading activation, so it writes heat exactly as a real `retrieve_memories` call does; and the owner
may release a held legacy reminder mapping on `/debug/reminder-mapping`. Tick **Dry run** for a strictly
read-only retrieval probe that writes nothing.

### Signing in, and what an account can see

`/debug/login` is the only unauthenticated page that reaches data — and it reaches none. Login,
invitation redemption and password recovery are the three public pages; each carries a short-lived,
single-use token issued with the page, so a bookmarked form cannot be replayed, and none of them is a
session. Signing in sets two cookies: `alexandria_session` (the credential) and `alexandria_csrf`
(the value every mutation must echo back). Both are `HttpOnly`, `SameSite=Lax`, `Path=/`, and `Secure`
unless the deployment is running under the explicit loopback-development exception below.

**Every mutation on this server requires the CSRF pair** — the Query Tester's run, reminder mapping
release, sign-out, token issue and revoke, invitation creation (administrators) and recovery creation,
and suspension. The guard,
not the handler, is the single authority for that, and it is what the paragraph below used to lean on
`Sec-Fetch-Site` alone for. An account can also mint and revoke its own agent tokens from
`/debug/account`, which is the same credential surface the MCP `Authorization` header consumes.

- **Account** (`/debug/account`) — the signed-in account's own credentials: browser sessions and agent
  tokens by label and creation time, with revoke, plus a recovery link for **this** account's password.
  A newly minted token's or link's secret is shown **once**, in the response that created it; afterwards
  the page lists the label and its last-used time, never the value. Creating an invitation is not
  here — see Admin
- **Admin** (`/debug/admin`, administrators only) — the account table: status, role, credential and
  memory counts, and the actions that suspend, activate, invite and reset. An ordinary owner gets
  `403` here, and the capability that gates it is a type the storage layer only issues after re-reading
  `account.role`, so a demotion cannot be outrun by a page rendered while the account still had it
- **Maintenance** (`/debug/maintenance`, administrators only) — the scheduler's **aggregate** health:
  one row per `run_id` with its jobs, the action verbs it ran and how many rows each touched. No record
  identities, no stored values and no per-row dispositions — a disposition lives in the audit table, not
  on this page. See the note under the page list below for why this replaced the per-row audit page

An administrator's own memories, sessions and clusters are read through the same owner-scoped path as
anybody else's: `AdminCapability` authorizes account management and nothing else, and there is no
route on this server that lists another account's content.

Two consequences of that boundary are deliberate and worth stating before they surprise anyone:

- **A fresh, empty store cannot create its first account or administrator through the browser.**
  Enrollment is invitation-only, and an invitation needs an administrator. The operator command that
  bootstraps the first account is the deployment step that comes next; until it exists, a new store's
  debug UI is reachable only as the page that says it cannot sign anybody in. Nothing about stdio,
  loopback, a `Host` header or the absence of accounts grants authority — authority comes from a
  credential and nothing else.
- **Owner-facing per-record history is not on this page yet.** The rows the old audit page listed still
  exist in `maintenance_log`, and the rows written before attribution landed carry no `run_id`, no
  `job` and no actor; they are counted into an "unattributed" group rather than shown to a browser role
  as record identities, because revealing which memories the scheduler collapsed is another account's
  content seen through an operator's page. A per-record history for owners is the next surface, over the
  operation journal.

### The pages

- **Dashboard** (`/debug`) — the caller's own record counts (facts active/deleted, clusters, edges, raw
  documents, sessions), the **effective configuration** the server is actually running with (retrieval
  floor, activation knobs, cluster and heat thresholds, the dreaming scheduler's cadences, live
  embedding model and dimensions) shown alongside
  what the TOML asked for, rollups for cluster health, sessions finalized/idle, top tags and heat
  distribution, and the schema version — applied versus compiled-in, with a mismatch called out. Every
  number is the signed-in population's, not the store's: this page used to call `stats::gather`, which
  counted every account's rows into one operator's summary
- **Memories** (`/debug/memories`) — paginated, sortable search/filter of **the caller's** facts by
  content and tag, with **include deleted** and **include quarantined** checkboxes; click through to a
  detail view showing heat, stability, timestamps, cluster membership, and graph edges. Quarantine is
  off by default so the page matches what an agent can retrieve, and this is the operator view for a
  memory on that middle rung — hidden from every retrieval path, still present, still reversible.
  Nothing in the UI can put a row into quarantine or take it out. A detail id that belongs to somebody
  else answers `404` exactly as an absent one does, which is the point: a distinguishable answer would
  be a way to test which record ids exist
- **Clusters** (`/debug/clusters`) — the caller's clusters with live member counts and depth; drill into
  `/debug/clusters/{id}` for member facts and cohesion. Cohesion is computed from each cluster's stored
  centroid, on the detail page and in the dashboard rollup alike, so the verdict matches what
  the background maintenance task will actually act on. A cluster the caller's population does not
  contain is a `404`, including one whose row is gone but whose edges still carry member facts: those
  edges hold other people's content, and labelling them would be the leak, not the diagnosis
- **Sessions** (`/debug/sessions`) — the caller's sessions with agent, model, derived memory count,
  start and last-activity times, and a finalized/active badge; `/debug/sessions/{external_id}` shows
  the summary, tags, and the session's memories. Keyed by the `external_id` you pass to
  `store_memory(session_id)`
- **Graph** (`/debug/graph/{id}`, JSON at `/debug/api/graph/{id}`) — visualizes **one of the caller's**
  memories' local edge neighborhood: nodes labelled with content snippets rather than raw record ids
  (full id and hop distance in the tooltip), colour and shape by table, edges styled by `edge_type`
  with strength banded into width, a legend generated from those same maps, and click-through to the
  memory detail page. `?hops=1..3` sets the radius, clamped server-side; the view caps at 200 nodes and
  says so when it truncates rather than rendering a hairball. Only edges whose **both** ends are inside
  the caller's population are drawn, and the labels are read back through the same scoped repo that
  produced the node list — a walk filtered by owner and labels fetched without that filter would put
  another account's text on the canvas
- **Maintenance** (`/debug/maintenance`, administrators only) — scheduler health as aggregates: runs
  and what each did, with no fact, session or cluster identities and no content. This deliberately
  replaced the pre-Task 9 page that listed every `maintenance_log` row across the whole store, because
  those rows name records belonging to accounts that were never given an operator role, and the rows
  written before attribution carries a `NONE` run id that cannot be shown to one owner without showing
  it to all. An owner asking *why did my memory move* is answered by the operation journal, not here
- **Reminder mapping** (`/debug/reminder-mapping`) — owner-only review of every held legacy target, including
  original free-text target and an exact hold count. Releasing a hold requires a revision-current choice
  of Personal or an owned project; it does not rewrite the historical target or reset schedule/delivery state.
- **Refinement** (`/debug/refinement`) — owner/workspace-filtered proposal queue and detail pages, with selected
  source records, proposed changes, rationale, uncertainty, model-policy comparison and linked private
  application pre/postimages/revert status. The settings page discloses endpoints and model names (never
  credentials), exposes revision-checked consent/revocation and review-mode controls, and makes human review
  the default. Unassigned proposals are marked and filterable; classification may stale pending work.
  Enqueue/attempt polling and proposal decision/apply/revert actions remain unavailable until the durable
  Task 19/20 worker and application services are integrated, so opening a page never starts inference or
  applies a proposal. Queue quota/attempt counts are explicitly unavailable rather than estimated.
- **Query Tester** (`/debug/query`, `POST /debug/query/run`) — run `retrieve_memories`/`recall` live
  against the real embedding model to sanity-check retrieval quality, from inside the signed-in
  account's own population, with optional `session_id` scoping and a **Dry run** checkbox. Results
  report the effective `min_similarity` floor, list what
  that floor dropped, whether spreading activation fired and on which top-N, and carry a measured
  score-band legend so a similarity number can be read without a ruler

All browser assets (htmx, vis-network) are **vendored** into the binary and served from
`/debug/assets/{name}`, so the UI loads nothing from a CDN and works fully air-gapped. The asset
allowlist is the only public surface besides the three auth pages, and it is a closed `match` over
literal filenames, so traversal is impossible by construction.

### Guards, and what they are for

The debug UI is no longer a thing you must put a proxy in front of to make it safe to publish, but the
proxy advice still applies to `/mcp`: an agent token is a bearer credential, and TLS terminates at the
reverse proxy you configure. What the server enforces itself:

- **Authentication is fail-closed.** `debug::router(server)` and `debug::router_with_context(server,
  Some(ctx))` — the two builds that used to be "the debug UI, unauthenticated" — now serve the asset
  allowlist and refuse **every** private route with `403` *before* any handler runs, so a route mounted
  above the guard, or a router assembled without a boundary, cannot leak. The production build is
  `router_secured(server, ctx, boundary)`, and `main.rs` is its only caller. A `403` here is a refusal,
  not an error page: it renders no markup, so a page cannot be probed for which routes exist.
- **`Host` and `Origin` are checked against the published origin.** Binding to loopback is not by
  itself a security boundary: DNS rebinding lets a remote page resolve its own domain to `127.0.0.1` and
  read `/debug/sessions` as same-origin. Under a secured build, `allowed_hosts` must contain the request
  `Host` and `allowed_origins` must contain `server.public_origin` exactly; a request with no `Host` is
  refused. The wildcard `["*"]` remains an option only for the legacy no-boundary build, which shows
  nothing anyway.
- **CSRF is a session property, not a header heuristic.** `Sec-Fetch-Site` is still read — a
  cross-site or cross-origin *mutation* is refused outright, and the check is unconditional — but a
  mutation now also needs the token that matches the caller's own session, in the `csrf` form field or
  the `X-CSRF-Token` header. `OPTIONS` is not exempt: this router advertises no CORS configuration, so a
  preflight here is the attack rather than a use case. A direct top-level navigation (`Sec-Fetch-Site:
  none`) still needs the token too, which is why a cross-site form cannot reach the Query Tester's heat
  write even with `Lax` cookies present.
- **Private and auth-page responses carry `no-store`**, so a shared machine's cache cannot hold another
  account's memory text.
- **Sessions expire on idle**, and an agent token's revocation is noticed on the next request rather
  than at some later reconnect, because the guard re-reads the live rows through the same `AuthService`
  that `/mcp` admits through.

Set `server.public_origin` to the `https` origin the UI is published at. Refusing to start without it —
rather than booting a browser-facing server on an inferred origin — is the point of the check;
`server.loopback_development = true` is the one exception, and it is honoured only on a loopback bind.
`[server] allowed_hosts` and `allowed_origins` must cover that origin, and see
[docs/configuration.md](docs/configuration.md) for the exact rules and the failure messages. Forwarded
headers are never consulted: `X-Forwarded-Proto`, `X-Forwarded-Host`, `X-Forwarded-For` and `Forwarded`
cannot establish the origin, the host, or the loopback exception, because a client that can reach the
port can set all four.

## Deployment

**Upgrading from a build on SurrealDB 3.2 is one-way: stop the server and copy the data dir
first** (`$XDG_DATA_HOME/alexandria/data` by default, `/data/db` in the container). A 3.3 build
opens a 3.2 store, but ordinary use soon writes data 3.2 cannot read. Listing a cluster or session
with 64 or more members makes 3.3 rewrite that table's definition, and every `store_memory` lists
every cluster. How close that leaves a given deployment depends on its largest cluster, so check
yours before upgrading — `/debug/clusters` prints the member count per cluster. Measured on a
1897-memory store: 1474 clusters ranging from 1 to 51 members, median 1, 1277 of them singletons,
so nothing is at the threshold yet; `merge` is the job that grows the top of that distribution
toward 64. Writes to the vector index leave entries 3.2 cannot decode. A 3.2 build started on
the result still boots, then fails cluster queries and vector search, or returns empty member lists
with no error. Restoring the copy is the only way back, as SurrealDB's own
[upgrade notes](https://github.com/surrealdb/surrealdb/blob/v3.3.0/doc/UPGRADING.md) say.

### As a systemd user service (recommended)

```ini
# ~/.config/systemd/user/alexandria.service
[Unit]
Description=Alexandria Agent Memory MCP Server
After=network.target

[Service]
ExecStart=%h/.cargo/bin/alexandria
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info,rmcp=warn

[Install]
WantedBy=default.target
```

`rmcp=warn` drops the per-request transport chatter (~6 lines per call at `info`) while keeping Alexandria's own logs.

journald has no per-unit size cap, so bound the journal globally if you want to limit history:

```ini
# /etc/systemd/journald.conf.d/alexandria.conf
[Journal]
SystemMaxUse=200M
MaxRetentionSec=1month
```

```bash
sudo systemctl restart systemd-journald
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now alexandria
journalctl --user -u alexandria -f  # tail logs
```

### In Docker

The [`Containerfile`](Containerfile) at the repo root builds a musl-targeted release binary and
ships it on a minimal Alpine runtime as a non-root user (`alexandria`, uid 10001). musl is linked
*dynamically* — the build clears `crt-static` because proc-macro and `cc`-based crates misbehave with
the musl target's default static CRT. Build and run:

```bash
docker build -f Containerfile -t alexandria .
docker run -d --name alexandria \
  -p 3000:3000 \
  -v alexandria-data:/data \
  alexandria
```

The image defaults to HTTP transport on `0.0.0.0:3000` via `ALEXANDRIA_SERVER_*` env vars, with
`ALEXANDRIA_DATA_DIR=/data/db`. Everything stateful lives under the `/data` volume:

| Path | Contents |
| --- | --- |
| `/data/db` | SurrealKV database — all memories, clusters, edges |
| `/data/hf-cache` | HuggingFace model cache (~80MB), so the first-run download survives rebuilds |

Override config per-container with env vars (see [docs/configuration.md](docs/configuration.md)) or
mount a TOML file and point `ALEXANDRIA_CONFIG` at it. Note the server binds `0.0.0.0` in the
image and the **MCP endpoint is authenticated by bearer token while the debug UI is authenticated by
account session** — publish the port only behind a reverse proxy that terminates TLS, because the token
and the session cookie both travel as bearer secrets and `server.public_origin` must name the `https`
origin clients actually reach. The debug UI needs no outbound network: its browser assets are
vendored into the binary, so a container with no egress still renders it (the first-run HuggingFace
model download is the only thing that still needs the internet).

### MCP client configuration

#### Generic (any MCP client)

```json
{
  "alexandria": {
    "type": "http",
    "url": "http://127.0.0.1:3000/mcp"
  }
}
```

Or via stdio (for single-session use):

```json
{
  "alexandria": {
    "command": "alexandria",
    "args": []
  }
}
```

#### In Claude Code

```bash
# Configure the native MCP client with an authorized bearer credential as required by the server.
claude mcp add --transport http --scope user alexandria http://127.0.0.1:3000/mcp
# Optional: explicit tool-use guidance.
cp -r contrib/claude/skills/alexandria-memory ~/.claude/skills/
# Optional: hook recall uses ALEXANDRIA_API_TOKEN; see contrib/claude/README.md.
cp contrib/claude/hooks/alexandria-recall.sh ~/.claude/hooks/
```

#### In pi

Pi 0.99 and later read MCP servers from `~/.pi/agent/mcp.json` (or a trusted project's
`.pi/mcp.json`) with built-in support, so a long-running Alexandria is a one-entry change there:

```json
{
  "mcpServers": {
    "alexandria": {
      "url": "http://127.0.0.1:3000/mcp",
      "headers": { "Authorization": "Bearer ${ALEXANDRIA_API_TOKEN}" },
      "exposure": "direct"
    }
  }
}
```

`"exposure": "direct"` is the setting that matters for a memory server you expect to be used on every
task. Pi's default is `codemode`, which keeps the tools out of the model's declared tool list and makes
them reachable only from JavaScript the model writes for its `codemode` tool; `deferred` is the middle
option, where `tool_search` loads matches on demand. An agent that has to *remember to look* for the
memory tools tends not to, so declare them.

Pi connects at session start, retries transient HTTP failures twice, and reconnects a dropped server on
the next call — there is no idle-eviction or keep-alive knob to tune (the earlier `lifecycle` and
`settings.idleTimeout` fields belong to the third-party `pi-mcp-adapter` extension, which reads
`mcp-adapter.json` and is not needed on 0.99+). Check the wiring with `pi mcp list`; for servers that
use OAuth, `pi mcp login <server>`. If a running session does not see a new or edited server, run
`/reload` or start a new session.

Pi names these tools `mcp__<server>__<tool>` — `mcp__alexandria__store_memory`,
`mcp__alexandria__retrieve_memories`, and so on — which is the form the skill uses. The native
connection above uses pi's standard MCP configuration; the separate companion extension uses
`ALEXANDRIA_API_TOKEN` and its own prompt-path configuration.

Then optionally add the client-side nudges — see
[`contrib/pi/README.md`](contrib/pi/README.md).

## Configuration

Config loads with precedence: defaults → `$XDG_CONFIG_HOME/alexandria/config.toml` → `ALEXANDRIA_CONFIG` env → individual env vars. Data defaults to `$XDG_DATA_HOME/alexandria/data`.

Legacy `~/.alexandria/config.toml` is used as a fallback when the XDG config file does not exist yet. The data directory has no such fallback: it defaults to `$XDG_DATA_HOME/alexandria/data`, and an explicit `data_dir` still pointing at the old location only produces a warning telling you to move it.

Reminder delivery is configured by the `[reminders]` section: `timezone` (IANA name, empty = system-local) governs naive datetime input and pattern/cron evaluation, and `escalation_hours` controls overdue escalation to global delivery.

The Pi companion extension (automatic recall and reminder checks; no automatic memory writers) has its own client config at `$XDG_CONFIG_HOME/alexandria/client.toml` or `~/Library/Application Support/alexandria/client.toml` on macOS. `ALEXANDRIA_CLIENT_CONFIG` selects another file; `ALEXANDRIA_API_TOKEN` and `ALEXANDRIA_PROJECT_ID` configure its authentication and project override, and `[transport]` / `ALEXANDRIA_PROMPT_CALL_TIMEOUT_MS` / `ALEXANDRIA_PROMPT_BUDGET_MS` configure its bounded prompt path. These client settings do not change the server configuration.

See [docs/configuration.md](docs/configuration.md) for all options, client config reference, and migration instructions.

## Documentation

| Document | Contents |
| --- | --- |
| [docs/configuration.md](docs/configuration.md) | Every server and client config key, env overrides, XDG migration |
| [docs/session-memory.md](docs/session-memory.md) | Session data model, lifecycle, tool semantics, current limitations |
| [docs/minilm-test-data.md](docs/minilm-test-data.md) | Historical MiniLM retrieval measurements: how to rerun `bench-retrieval`, metric definitions, the floor rule and the client threshold sweep |
| [docs/granite-test-data.md](docs/granite-test-data.md) | Granite public multi-relevant/unanswerable pilot, native offline evaluation, model-specific cutoff caveats and release gates |
| [docs/roadmap.md](docs/roadmap.md) | Shipped milestones, known gaps, planned work |
| [contrib/pi/README.md](contrib/pi/README.md) | pi skill vs. extension: what each does, install, failure behavior |
| [contrib/pi/extensions/alexandria/README.md](contrib/pi/extensions/alexandria/README.md) | The pi companion extension: authenticated recall/reminder behavior, project context, configuration and known gaps |
| [contrib/pi/skills/alexandria-memory/SKILL.md](contrib/pi/skills/alexandria-memory/SKILL.md) | The pi client-side skill (`mcp__alexandria__<tool>` names) |
| [contrib/claude/README.md](contrib/claude/README.md) | Claude Code skill and hooks: install, `settings.json`, requirements |
| [contrib/claude/skills/alexandria-memory/SKILL.md](contrib/claude/skills/alexandria-memory/SKILL.md) | The Claude Code skill (`mcp__alexandria__<tool>` names) |
| [docs/security-findings.md](docs/security-findings.md) | 2026-09-10 security audit: threat model, the convex-hull paper verdict, findings S1–S6 with current status |
| [docs/performance-and-ability-findings.md](docs/performance-and-ability-findings.md) | Same audit: performance and retrieval-ability findings A1–A4 / P1–P5 |
| [docs/prompt-path-stall-attribution.md](docs/prompt-path-stall-attribution.md) | 2026-09-22 investigation record: prompt-path stall attribution, the measured cold-handshake fault, and why the worker isolation it proposed was dropped. Carries a STATUS banner naming what shipped |
| [crates/alexandria-mcp/assets/README.md](crates/alexandria-mcp/assets/README.md) | Vendored debug-UI assets, checksums, how to re-vendor |
| [TODO-misc.md](TODO-misc.md) | Unprioritised backlog, grouped by area |
| [AGENTS.md](AGENTS.md) | Working notes for humans and agents on this codebase — SurrealDB 3.3 gotchas, crate boundaries, task runner |

## Architecture

```text
crates/
├── alexandria/          # Binary — config loading, transport setup, main loop, dreaming scheduler
├── alexandria-mcp/      # MCP tool handlers + debug web UI (askama templates, vendored assets)
├── alexandria-engine/   # Core algorithms — clustering, heat, recall, import, activation
├── alexandria-pipeline/ # Embedding providers (legacy Candle + native Granite/Burn)
└── alexandria-storage/  # SurrealDB connection, models, repos, schema migrations

contrib/pi/              # Optional client-side pi integrations (skill + extension), not shipped
```

Data flows: **MCP request → server handler → engine algorithm → storage repo → SurrealDB**

Crate boundaries are held by convention, not tooling: `storage` owns nearly all SurrealDB access (one
inline query remains in an `alexandria-mcp` handler, and the binary's maintenance loop reads `cluster`
directly), `engine` is pure algorithms with no DB or async, `pipeline` abstracts embedding providers
behind one trait, and `alexandria-mcp` is the crate whose purpose is joining engine to storage — the
binary also sees both, since it wires them together at startup. See [AGENTS.md](AGENTS.md) for the full
set.

## Development

`just` recipes mirror what CI runs — install [just](https://just.systems/) if you don't have it:

```bash
just               # list every recipe
just ext-install   # ONE TIME per checkout: npm ci --ignore-scripts for the pi companion (node_modules/ is gitignored)
just test          # cargo test --locked --workspace --all-features (hermetic Granite tests; native runs ignored)
just test-cpu      # CPU-only workspace tests, including uncompiled-GPU admission guards
just cpu-deps      # CPU binary graph rejects known GPU backend/native dependencies
just artifact-ci   # standalone curator fmt/clippy/hermetic tests/cargo-deny, no official model conversion
just eval-test     # dependency-free Python scorer tests; no inference
just ext-test      # pi companion: npm run typecheck + npm test (needs ext-install first)
just lint          # clippy, warnings as errors (matches CI)
just fmt-fix       # rustfmt
just ci            # CPU graph + both Rust feature sets + curator/scorer/companion/policy/assets
just run           # run the server locally (prefix with RUST_LOG=debug for verbose logs)
just verify-assets # check crates/alexandria-mcp/assets against SHA256SUMS
just vendor-assets # re-download those assets and re-verify the checksums
```

`just ci` runs `ext-test`, so **`just ext-install` is a prerequisite on a fresh checkout** — without it
the "full pre-push check" fails in npm before it reaches anything Rust. Bare `just` lists the recipes
this block omits (`fmt`, `check`, `clean`, `test-all`, `deny`).

Use a **stable** toolchain. Recent nightlies fail to build `diskann-wide` (a SurrealDB
transitive dependency) on Apple Silicon with a trait-inference error in its NEON intrinsics,
which looks like a breakage in this repo but isn't.

Install the git pre-commit hook (fmt check + clippy) once per clone:

```bash
just install-hooks
```

[`deny.toml`](deny.toml) gates licenses and known advisories via `cargo deny`, which runs as its own
CI job. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs five jobs — `fmt`, `clippy`,
`test`, `extension` (displayed as "Pi companion": the extension's typecheck and tests) and `deny` — on
PRs against any branch and pushes to `main`; it does not call `just ci`, so a new local gate has to be wired
into the workflow as well. `just verify-assets` runs in the `test` job ahead of `just test`.
Curator fmt/Clippy/tests/dependency-policy gates run in the corresponding existing jobs; the
`test` job also runs CPU-only workspace and Python scorer tests. Optional GPU backend compilation
is not a hardware acceptance pass: native GPU tests require explicitly verified local assets
and supported hardware. No official Granite model download/conversion runs in ordinary CI.
The `test` job disables debug info (`CARGO_PROFILE_DEV_DEBUG=0`,
`CARGO_PROFILE_TEST_DEBUG=0`) and incremental compilation (`CARGO_INCREMENTAL=0`) to
fit both feature sets and the curator on the hosted runner. Optimization levels,
debug assertions, overflow checks and test coverage are unchanged; CI backtraces
have less detail. Local builds retain their normal debug settings.

During dependency refreshes, retain the resolved SurrealDB and Burn families in
addition to their exact top-level pins: pinning the SurrealDB SDK alone does not
freeze its internal engine crates. Engine changes need the deployment/storage-format
review described below; inference-stack changes need artifact and vector validation.

[`.github/workflows/container.yml`](.github/workflows/container.yml) additionally builds the Docker
image and boot-tests it — waits on `/debug`, then performs a real MCP `initialize` handshake against
`/mcp` — but only when a change touches the `Containerfile`, `.dockerignore`, the manifests, or
`crates/**`. A green `CI` run therefore says nothing about the image, and vice versa.

## License

Distributed under **AGPL-3.0-or-later** — see [`LICENSE`](LICENSE) and the `license` field in
[`Cargo.toml`](Cargo.toml). AGPL rather than MIT is deliberate: Alexandria is a long-running
network service, and the AGPL source-availability clause closes the network-service loophole that
MIT would leave open for anyone hosting a modified fork.
