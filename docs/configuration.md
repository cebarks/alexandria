# Configuration Reference

Alexandria loads server config with this precedence:

1. **Built-in defaults**
2. **Config file** — exactly one file is loaded, chosen by first-match priority:
   - `ALEXANDRIA_CONFIG` env var (explicit path override)
   - `$XDG_CONFIG_HOME/alexandria/config.toml` (default: `~/.config/alexandria/config.toml` on Linux, `~/Library/Application Support/alexandria/config.toml` on macOS)
   - `~/.alexandria/config.toml` (legacy fallback, logged with a warning)
3. **Individual env vars** — `ALEXANDRIA_SERVER_TRANSPORT`, `ALEXANDRIA_SERVER_HOST`, `ALEXANDRIA_SERVER_PORT`, `ALEXANDRIA_DATA_DIR`, `ALEXANDRIA_EMBEDDING_MODEL`, `ALEXANDRIA_EMBEDDING_DEVICE`, `ALEXANDRIA_EMBEDDING_BATCH_SIZE`, `ALEXANDRIA_EMBEDDING_MAX_TOKENS`, `ALEXANDRIA_REMINDERS_TIMEZONE`, `ALEXANDRIA_REMINDERS_ESCALATION_HOURS`

## Full Example

```toml
[server]
transport = "http"            # "stdio" or "http" (default: "stdio")
host = "127.0.0.1"            # HTTP bind address (default: "127.0.0.1")
port = 3000                   # HTTP port (default: 3000)
allowed_origins = ["*"]       # CORS origins; ["*"] disables validation (default: ["*"])
allowed_hosts = ["*"]         # Allowed Host headers; ["*"] disables validation (default: ["*"])
sse_keep_alive_secs = 15      # SSE keep-alive interval in seconds (default: 15)

[database]
# data_dir = "/home/you/.local/share/alexandria/data"  # Storage path; ":memory:" for ephemeral (default: $XDG_DATA_HOME/alexandria/data)

[embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"   # HuggingFace model ID (default shown; omit to use it)
device = "cpu"                                       # "cpu" only for now (default: "cpu")
batch_size = 32                                      # Facts per embed() call in migrate-embeddings (default: 32)

[heat]
decay_tau_secs = 86400.0          # Base decay time constant, seconds; tau = stability * this (default: 86400 = 1 day)
spacing_reference_secs = 86400.0  # Access gap earning full stability growth, seconds (default: 86400 = 1 day)

[activation]
propagation_factor = 0.3   # Fraction of heat passed per hop (default: 0.3)
max_hops = 2               # Maximum graph hops for spreading activation (default: 2)
top_n = 3                  # Number of top retrieval results that trigger spreading activation (default: 3)

[cluster]
join_threshold = 0.75              # Cosine similarity threshold to join existing cluster (default: 0.75)
merge_threshold = 0.9              # Centroid similarity above which two clusters merge (default: 0.9)
cohesion_floor = 0.6               # Avg member-to-centroid similarity below which a cluster splits (default: 0.6)

[dreaming]
enabled = true                     # Master switch for the background scheduler (default: true)
sweep_interval_secs = 3600         # Heat materialisation cadence (default: 3600 = 1 hour)
cluster_interval_secs = 300        # Cohesion check → split cadence (default: 300 = 5 minutes)
merge_interval_secs = 300          # Centroid similarity → merge cadence (default: 300 = 5 minutes)
collapse_interval_secs = 86400     # Byte-identical duplicate collapse cadence (default: 86400 = 1 day)
appraise_interval_secs = 86400     # Cold-row demotion cadence (default: 86400 = 1 day)
max_rows_per_run = 500             # Rows one job may write per run (default: 500)
cold_heat_floor = 0.05             # Projected heat at or below which a memory counts as cold (default: 0.05)
demote_confidence_ceiling = 0.5    # Confidence at or below which a cold, never-accessed memory is demotable (default: 0.5)

[retrieve]
min_similarity = 0.10              # Server-side hard floor on cosine similarity for retrieve_memories (default: 0.10)

[reminders]
timezone = "Europe/Stockholm"      # IANA name for naive datetimes + pattern/cron evaluation; "" = system-local (default: "")
escalation_hours = 48              # Project-targeted reminders escalate to global delivery after being overdue this many hours; 0 = escalate as soon as overdue (default: 48)
```

## Section Details

### `[server]`

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `transport` | string | `"stdio"` | Transport protocol. `"stdio"` for direct pipe, `"http"` for persistent HTTP service. |
| `host` | string | `"127.0.0.1"` | Bind address for HTTP mode. Use `"0.0.0.0"` to listen on all interfaces. |
| `port` | u16 | `3000` | Port for HTTP mode. |
| `allowed_origins` | string[] | `["*"]` | CORS allowed origins. `["*"]` disables origin validation. Set to specific origins (e.g. `["http://localhost:3000"]`) in production. |
| `allowed_hosts` | string[] | `["*"]` | Allowed HTTP Host header values. `["*"]` disables host validation. |
| `sse_keep_alive_secs` | u64 | `15` | SSE keep-alive interval in seconds. Controls how often the server sends keep-alive pings on Streamable HTTP connections. |

### `[database]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `data_dir` | path | `$XDG_DATA_HOME/alexandria/data` | SurrealKV storage directory. Set to `":memory:"` for ephemeral in-memory storage (data lost on restart). Default is `~/.local/share/alexandria/data` on Linux, `~/Library/Application Support/alexandria/data` on macOS. |

The data directory contains SurrealKV files (LOCK, manifest, sstables, vlog, wal). Back up this directory to preserve all memories.

### `[embedding]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `model` | string | `"sentence-transformers/all-MiniLM-L6-v2"` | HuggingFace model ID. Must be a BERT-family model compatible with candle. Pooling mode (CLS or mean) is read from the model repo's `1_Pooling/config.json`; models without it use mean pooling. |
| `device` | string | `"cpu"` | Compute device. Only `"cpu"` is currently supported. |
| `batch_size` | usize | `32` | Facts per `embed()` call during `alexandria migrate-embeddings`. Sets how often the migration writes and logs progress; it does not bound memory or change speed with the Candle provider, which runs one forward pass per text. Must be between 1 and 4096, checked at config load. The server itself embeds one text at a time. |
| `max_tokens` | usize | `256` | Longest text one embedding sees, in wordpiece tokens including `[CLS]`/`[SEP]`. A longer memory is still stored whole, but only its first `max_tokens` tokens are searchable; the server logs a warning with the fact id and token count, and `store_memory` returns `truncated: true`. **256** is the default and the measured value: it is what sentence-transformers serves this model at, and it covers all but the longest ~1.7% of the corpus it was measured on — that corpus's p99 is 284 tokens, so 256 falls just short of it; see [docs/minilm-test-data.md](minilm-test-data.md), "256-token re-embed". **128** is what the model's `tokenizer.json` ships, so every database created before this key existed is stamped 128 and **must be migrated before it will boot at the default** (see Model locking below). Anything above 256 is experimental: the model was trained at 128 and nothing here has measured it — issue #26 proposes the sweep that would set this from a curve instead of a convention. Must be between 3 and 512, checked at config load, and no larger than the model's position table, checked when the model loads. Padding is off at every value, so a text costs its own length, not the limit. |

**Switching models on an existing database:** stop the server, set the new `model`, run `alexandria migrate-embeddings` (re-embeds every memory and cluster centroid, then updates the lock), and start the server again. Thresholds (`[cluster]`, `[retrieve] min_similarity`, and the client's `[recall] min_similarity`) are tuned to the default model; retune them if you switch. `alexandria bench-retrieval` derives the latter two from the new model's own output — see [docs/minilm-test-data.md](minilm-test-data.md). The migration is not transactional: if it fails partway, rerun it. Do not revert `model` in config afterwards, the database may hold a mix of old and new vectors.

**Raising the token limit on an existing database:** stop the server, set `max_tokens`, run `alexandria migrate-embeddings`, start the server. It re-embeds everything, as a model switch does, and moves the lock last. The limit only goes up: `migrate-embeddings` refuses a `max_tokens` below what the corpus is locked at, and the server refuses to boot on one, so the way back from 256 is to keep `max_tokens = 256`. `alexandria migrate-embeddings --force` re-embeds even when the lock already matches config, for a corpus whose lock is right and whose vectors may not be.

**A migration performed before this key existed is invisible to the lock, and `--force` is the repair.** The token lock landed after the key did; a corpus re-embedded at 256 *before* that release has no `embedding_max_tokens` row, so it reads back as 128 and every check agrees with itself while describing the wrong corpus. The symptom is a server that boots clean and logs `text exceeds 128 tokens` for facts a 256-token pass had already embedded whole, leaving the corpus mixed. Nothing detects this — the lock cannot know about a migration that predates it. If the install's history includes a pre-lock re-embed, run `alexandria migrate-embeddings --force` once to re-embed at the configured limit and stamp the lock honestly.

**Model locking:** On first boot, the model name, dimension count, and token limit are stored in the database. Changing the model or the limit in config without migrating causes a startup error that says which way to go. A database locked before the token limit was recorded, or one that has memories and no lock at all, was embedded at 128 tokens and is treated as locked at 128: it boots at the default and refuses a higher `max_tokens` until `alexandria migrate-embeddings` has run.

**Rolling back the binary is unsafe after a migration, and undetectable.** A release from before `max_tokens` existed does not read the token lock. Run against a database migrated to 256, it writes 128-token, padded vectors into a corpus labelled 256, and nothing can tell afterwards. From this release on, a binary refuses to open a database whose schema version is newer than its own, which stops the general case; it cannot stop binaries that predate the check. If it has happened, `alexandria migrate-embeddings --force` repairs it.

**First run:** The model weights (~80MB for all-MiniLM-L6-v2) are downloaded from HuggingFace Hub and cached in `~/.cache/huggingface/`.

### `[heat]`

Both values are one day by default, which is the number the engine hardcoded before they existed — so at default the curve is arithmetically unchanged. `projected_heat` takes `decay_tau_secs` and `on_access` takes `spacing_reference_secs` as parameters; `DEFAULT_DECAY_TAU_SECS` / `DEFAULT_SPACING_REFERENCE_SECS` in `alexandria_engine::heat` are the single home for both defaults, derived into `HeatConfig` and `HeatSettings` and guarded by `test_server_fallback_defaults_match_config_defaults`.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `decay_tau_secs` | f64 | `86400.0` | Base decay time constant, seconds. The effective constant is `stability * decay_tau_secs`, so a memory with stability 2.0 cools twice as slowly. **Lower = cools faster.** This is an e-folding, not a half-life: at stability 1.0 one elapsed day leaves `heat/e`, pinned by `test_decay_tau_scales_the_curve`. |
| `spacing_reference_secs` | f64 | `86400.0` | The gap between two accesses at which the later one earns **full** stability growth; a shorter gap earns a proportional fraction (a burst of same-second accesses grows stability by almost nothing). **Lower = stability accrues from less widely spaced accesses, so heat cools slower.** |

**What each key actually governs.** `spacing_reference_secs` is the denominator `on_access` grows
`stability` with, and `retrieve_memories` records an access for every row it returns to the caller, so
it is live on the request path. `decay_tau_secs` is the time constant the `sweep` job materialises
with and `appraise` judges coldness by.

Neither affects **ranking**: retrieval still orders by cosine similarity alone, so the heat model
moves real numbers that no ordering reads yet. Audit finding A2 in
[performance-and-ability-findings.md](performance-and-ability-findings.md) records that and stays
open for exactly this reason — [#43](https://github.com/cebarks/alexandria/issues/43) wired the
input, not the output.

**Removed in this release:** `spacing_halflife_secs`. It named a half-life while being used only as the spacing denominator above, and its documented direction was the reverse of its behaviour — the name is how that happened, which is why neither replacement key uses the word. Because `[heat]` is `#[serde(default)]`, a leftover key in `config.toml` is otherwise **silently ignored**; the server instead warns at boot naming both replacements, and `a_config_using_the_removed_heat_key_is_warned_about` guards that. The old single control is now two, because the value it named had two effects that pull in opposite directions.

### `[activation]`

Controls spreading activation — when a memory is accessed, its graph neighbors receive a fraction of heat.

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `propagation_factor` | f32 | `0.3` | Heat fraction passed per hop. At hop 1, a neighbor gets `propagation_factor × edge_strength` of the source heat. At hop 2, `propagation_factor² × edge_strength`. |
| `max_hops` | u32 | `2` | Maximum graph traversal depth. Higher values spread activation further but cost more DB queries. |
| `top_n` | integer | `3` | Number of top retrieval results that trigger spreading activation. Only the top N results from `retrieve_memories` fire the activation side effect. |

### `[cluster]`

Controls automatic cluster assignment, splitting, and merging. The cadences themselves live in
`[dreaming]` — this section holds only the thresholds the jobs compare against.

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `join_threshold` | f32 | `0.75` | Minimum cosine similarity between a new memory's embedding and a cluster centroid to join that cluster. Below this, a new cluster is created. |
| `merge_threshold` | f32 | `0.9` | Centroid-to-centroid similarity above which two clusters are merged. Read by the `merge` job. |
| `cohesion_floor` | f32 | `0.6` | Average member-to-centroid similarity below which a cluster is split via k-means(k=2). Read by the `cluster` job. |

> **Removed:** `cluster.maintenance_interval_secs`. Cluster maintenance is no longer one loop with
> one cadence — it is two of the five dreaming jobs. A config file still setting it boots, but logs a
> warning naming `dreaming.cluster_interval_secs`, `dreaming.merge_interval_secs` and
> `dreaming.enabled`.

### `[dreaming]`

The background housekeeping scheduler: **one loop, five independently-due jobs**. HTTP mode only —
stdio has no long-lived process to run a clock in, so nothing here applies to a stdio server.

Each job carries its own interval and its own last-run stamp, so one job failing or running long
affects only itself. A process that was down for three intervals catches up with **one** run of each
job, not three: a missed tick costs latency, never correctness. The loop sleeps until the soonest
job is due rather than waking on a fixed tick.

| Job | What it does |
| ----- | ------------- |
| `sweep` | Materialises decayed heat so stored values are current, and reads the decay anchor forward. Writes no audit rows: it changes ranking by nothing, since every reader projects heat itself. |
| `cluster` | Cohesion check → split, using `cluster.cohesion_floor`. |
| `merge` | Centroid similarity → merge, using `cluster.merge_threshold`. Re-reads the cluster set after each merge, because a merge changes the centroids the next comparison would use. |
| `collapse` | Finds byte-identical duplicates, keeps one, soft-deletes the rest and links them to the survivor. |
| `appraise` | Demotes memories that are cold, never accessed **since access recording was armed**, and no more confident than the default. |

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `enabled` | bool | `true` | Master switch. `false` means the loop is never spawned, so no job writes anything. |
| `sweep_interval_secs` | u64 | `3600` | Heat materialisation cadence, seconds. |
| `cluster_interval_secs` | u64 | `300` | Cohesion-check cadence, seconds. Replaces `cluster.maintenance_interval_secs` for splits. |
| `merge_interval_secs` | u64 | `300` | Merge cadence, seconds. Split from `cluster` because merge is the expensive half — an operator who wants merges rarer should not pay for it in slower splits. |
| `collapse_interval_secs` | u64 | `86400` | Duplicate-collapse cadence, seconds. |
| `appraise_interval_secs` | u64 | `86400` | Demotion cadence, seconds. |
| `max_rows_per_run` | u64 | `500` | Rows one job may **write** per run. Not a bound on reads: `collapse` reads every live fact to group duplicates, and `appraise` reads every live fact's confidence and store time while paging only this many heat rows. `examined` in the job's trace line is what was read, so `examined: 12000, acted: 3` is a normal collapse. Must be ≥ 1. |
| `cold_heat_floor` | f64 | `0.05` | Projected heat at or below which a memory counts as cold. **Provisional**: a fraction of the `1.0` a fresh access writes, not a number derived from retrieval measurements. |
| `demote_confidence_ceiling` | f64 | `0.5` | Stored confidence at or below which a cold, never-accessed memory is demotable. Defaults to the confidence `store_memory` writes when the caller supplies none, so the rule reaches only memories nobody asserted more strongly AND nobody retrieved while recording was armed. Must be above `0.2`, the value demoted memories are written at. **Provisional**, as above. |

**Reach of the rule.** `Appraise` judges only memories stored at or after the boot that armed access
recording (`system_config::access_recording_armed_at`, stamped on first boot). Every older memory
reads as `access_count = 0` because nothing wrote that field before this release, so trusting the
count would walk an existing corpus down to `0.2` at 500 rows a day, oldest anchors first — the rows
most likely to have been used hardest. The cost of refusing is that pre-arming memories are exempt
permanently, which is the honest price of not demoting on a number nobody recorded.

Every interval must be ≥ 1 second: the engine treats a zero interval as "due on every tick", which
would turn a daily pass into a hot loop, so the server refuses to start on one and names the key.

There are no `ALEXANDRIA_DREAMING_*` environment overrides, and the gap is deliberate rather than an
oversight: these are cadences tuned in a file and applied at restart, and a partial set of env vars
(enabled but not the intervals, say) invites exactly the "why is there no env var for this" question
a complete set would answer. `dreaming` is not the only section in that position — `[heat]`,
`[activation]`, `[cluster]` and `[retrieve]` have no environment overrides either, so the table below
is the complete list of what the server reads from the environment, not an excerpt of a scheme that
covers every section.

Job runs are logged at `debug` with `examined` and `acted` counts. `cluster`, `merge`, `collapse` and
`appraise` each write a row to `maintenance_log` — visible at `/debug/maintenance` — and `sweep`
deliberately does not.

### `[retrieve]`

Controls server-side filtering of `retrieve_memories` results.

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `min_similarity` | f32 | `0.10` | Hard floor on cosine similarity below which results are dropped, regardless of the requested `limit`. A noise cutoff only — the client's `[recall] min_similarity` does the real filtering. Model-dependent: for `all-MiniLM-L6-v2` (measured 2026-09-08), a keyword or near-paraphrase hit scores 0.55–0.76, a natural-language question against its matching statement 0.40–0.65, and a question sharing no vocabulary with the statement as low as ~0.2. Unrelated memories score 0.07–0.40. `0.10` is a constant kept by hand. `alexandria bench-retrieval` prints a retrieve-floor rule — the median non-hit score rounded to two decimals, valid only if it sits below the weakest correct hit — but **its output is a property of the model *and* the corpus and does not move in one direction**: `0.08` at 143 facts, `0.07` at 807, back to `0.08` at 957. `0.10` sits above all of them and far below the weakest true hit on `all-MiniLM-L6-v2` (0.338), which is the whole argument for it. The recorded passes, and the re-measurement those bands need for any other model, are in [docs/minilm-test-data.md](minilm-test-data.md). |

The default is defined once at `alexandria_engine::search::DEFAULT_MIN_SIMILARITY`, which both
`RetrieveConfig::default()` and `AlexandriaServer`'s construction fallback read. The measured score
bands above are the same numbers the debug Query Tester renders; both come from one const,
`SCORE_BANDS_LEGEND` in `crates/alexandria-mcp/src/debug/query.rs`, and
`configuration_md_quotes_every_score_band` fails the build if this page stops quoting them, so
re-measure by editing the const and this table together.

### `[reminders]`

Controls how reminder schedules are interpreted and how project-targeted reminders are guaranteed to arrive. These keys affect the reminder tools only.

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `timezone` | string | `""` (system-local) | IANA timezone name (e.g. `"Europe/Stockholm"`) used to interpret naive datetimes passed to `set_reminder` and to evaluate recurring `pattern`/`cron` schedules (wall-clock semantics — a daily 09:00 stays 09:00 local across DST). Empty = the system-local timezone detected at startup, falling back to UTC if detection fails; an invalid IANA name is a startup error. Explicit ISO-8601 offsets in `due_at` are always honored regardless. Across a transition: a wall time removed by the spring-forward gap has no fire that day, and a time duplicated by a fall-back fold fires **once**, on the first pass — one wall-clock reading is one occurrence. Naive one-shot inputs that land in either zone are rejected at set time with a message naming the fix. |
| `escalation_hours` | u64 | `48` | How long a project-targeted reminder may stay overdue before `check_reminders` delivers it regardless of the caller's project (labeled `escalated: true`), so a project that stops being visited can never silently swallow its reminders. The boundary is inclusive: a reminder escalates once it is *at least* this many hours overdue, which is what makes `0` escalate every overdue project reminder. An unparseable value in the env override is a startup error, and so is a value too large to represent as a duration — the delivery path would otherwise hold project reminders forever, on every check. One default, `alexandria_engine::reminders::DEFAULT_ESCALATION_HOURS`, shared with the MCP server's fallback. |

## Environment Variable Overrides

These env vars override individual config values after the TOML file is loaded:

| Variable | Overrides |
| --- | --- |
| `ALEXANDRIA_CONFIG` | Path to an alternate config TOML file |
| `ALEXANDRIA_SERVER_TRANSPORT` | `server.transport` (`"stdio"` or `"http"`) |
| `ALEXANDRIA_SERVER_HOST` | `server.host` |
| `ALEXANDRIA_SERVER_PORT` | `server.port` — a non-numeric value fails startup with an error naming the variable |
| `ALEXANDRIA_DATA_DIR` | `database.data_dir` |
| `ALEXANDRIA_EMBEDDING_MODEL` | `embedding.model` |
| `ALEXANDRIA_EMBEDDING_DEVICE` | `embedding.device` |
| `ALEXANDRIA_EMBEDDING_BATCH_SIZE` | `embedding.batch_size` |
| `ALEXANDRIA_EMBEDDING_MAX_TOKENS` | `embedding.max_tokens` |
| `ALEXANDRIA_REMINDERS_TIMEZONE` | `reminders.timezone` |
| `ALEXANDRIA_REMINDERS_ESCALATION_HOURS` | `reminders.escalation_hours` — a non-numeric value fails startup with an error naming the variable |

The `ALEXANDRIA_SERVER_*` variables exist so a container can be configured entirely by environment
(the bundled [Containerfile](../Containerfile) uses them to default to HTTP on `0.0.0.0:3000`) without
shipping a config file.

Everything else — `[heat]`, `[activation]`, `[cluster]`, `[retrieve]`, CORS, and SSE keep-alive —
can only be set via the TOML file.

---

## Client Configuration

The Pi companion extension (recall / store / reminders) loads its own config from
`$XDG_CONFIG_HOME/alexandria/client.toml`. It is a separate file with separate keys — the server
never reads it and the extension never reads `config.toml`.

Precedence: defaults → `client.toml` → `ALEXANDRIA_CLIENT_CONFIG` env var (path to alt TOML) → individual `ALEXANDRIA_*` env vars.

### Full Example

```toml
[server]
url = "http://127.0.0.1:3000/mcp"

[recall]
enabled = true
limit = 10
min_similarity = 0.45

[store]
enabled = true
extract_model = "vertex/claude-haiku-4-5"
extract_timeout_ms = 5000

[reminders]
enabled = true
project = "alexandria"
```

### `[server]`

| Key | Type | Default | Env Override | Description |
|-----|------|---------|-------------|-------------|
| `url` | string | `"http://127.0.0.1:3000/mcp"` | `ALEXANDRIA_URL` | Alexandria MCP server endpoint URL. |

### `[recall]`

| Key | Type | Default | Env Override | Description |
| ----- | ------ | --------- | ------------- | ------------- |
| `enabled` | bool | `true` | `ALEXANDRIA_AUTO_RECALL=off` | Enable auto-recall on every prompt. |
| `limit` | number | `10` | `ALEXANDRIA_AUTO_RECALL_LIMIT` | Max memories to retrieve per prompt. It is not merely a cap — a target ranked below it cannot be surfaced by any threshold, so it is a recall lever in its own right, and the stronger of the two. A judgement call on a small hand-authored question set, not a measurement — the questions were written against facts already in the corpus and none lacks a target (see [docs/minilm-test-data.md](minilm-test-data.md), "Limitations"). What the 2026-09-09 limit × threshold grid on an 880-fact corpus showed ("Result limit"): delivery saturates at `10`, because the worst of the 12 benchmark target ranks is 9, so `15` and `20` add non-target memories and no hits. Was `5` until that pass, which hid 3 of the 11 targets that cleared the then-default threshold on score. Lowering it below `3` also silently narrows spreading activation (`activation.top_n`). |
| `min_similarity` | number | `0.45` | `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | Minimum cosine similarity to include an auto-recalled memory. A judgement call read off the same grid, with the same caveats; the grid counts how many of 12 known targets a threshold actually delivers *through* `limit`. **Read it together with `limit` — the two are not independent.** At `limit = 10`: `0.45` delivers 8/12 at ~1.0 non-targets per prompt, `0.35` delivers 11/12 at ~4.7, `0.50` delivers 7/12 at ~0.5, and the old `0.58` Pi default delivers only 4/12 — it assumed genuine matches score 0.6+ and drops two thirds of real hits. `0.40` was dominated on that 12-question grid (same 8 delivered as `0.45`, roughly double the noise); with the 20-question set it delivers 16/20 at ~3.0 against `0.45`'s 14/20 at ~1.65, a genuine trade that the strict-dominance rule used to pick the pair does not take. `0.45` is chosen as the frontier pick: paired with `limit = 10` it delivers what the previous `5`/`0.35` pair did at a third of the injection. Note that `0.45` is only on the frontier *because* the limit is 10 — at `limit = 5` it was dominated by `0.50`, so do not lower one without revisiting the other. It is not free: the weakest target scores 0.338 and falls below it. |

### `[store]`

| Key | Type | Default | Env Override | Description |
| ----- | ------ | --------- | ------------- | ------------- |
| `enabled` | bool | `true` | `ALEXANDRIA_AUTO_STORE=off` | Enable heuristic store detectors and LLM extraction. |
| `extract_model` | string | `"vertex/claude-haiku-4-5"` | `ALEXANDRIA_EXTRACT_MODEL` | Model for session-end LLM extraction. Falls back to session model if unavailable. |
| `extract_timeout_ms` | number | `5000` | `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | Timeout for the extraction LLM call in milliseconds. |

### `[reminders]`

Client-side delivery keys. The server-side `[reminders]` section above configures how
schedules are interpreted; this section only says whether the extension asks for due reminders, and
what project hint it sends with the question.

| Key | Type | Default | Env Override | Description |
| ----- | ------ | --------- | ------------- | ------------- |
| `enabled` | bool | `true` | `ALEXANDRIA_REMINDERS=off` | Call `check_reminders` on every prompt and inject whatever is due. The server runs no timer, so turning this off means reminders reach the user only if the agent calls `check_reminders` itself. |
| `project` | string | (git repo dir name) | `ALEXANDRIA_REMINDERS_PROJECT` | Project hint sent with each check, matched exactly (case-sensitive) against the `target_project` set by `set_reminder`. Defaults to the basename of `git rev-parse --show-toplevel`; set it when the checkout directory is not the project name, as with a git worktree. Unset and outside a repo, only global and escalated reminders are delivered. |

---

## Legacy Migration

Alexandria previously stored all files under `~/.alexandria/`. The new layout uses XDG Base Directory paths:

| What | Old Path | New Path |
|------|----------|----------|
| Server config | `~/.alexandria/config.toml` | `$XDG_CONFIG_HOME/alexandria/config.toml` |
| Database | `~/.alexandria/data/` | `$XDG_DATA_HOME/alexandria/data/` |

The server automatically falls back to the legacy paths if the XDG paths don't exist, with a warning log message suggesting migration. To migrate:

```bash
# Create XDG directories
mkdir -p ~/.config/alexandria
mkdir -p ~/.local/share/alexandria

# Move config
mv ~/.alexandria/config.toml ~/.config/alexandria/config.toml

# Stop Alexandria, move data, restart
mv ~/.alexandria/data ~/.local/share/alexandria/data

# Remove explicit data_dir from config.toml if it pointed to ~/.alexandria/data
# (the new default is $XDG_DATA_HOME/alexandria/data)
```

Once confirmed working, `~/.alexandria/` can be removed.
