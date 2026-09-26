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
spacing_halflife_secs = 86400.0   # Spaced repetition half-life in seconds (default: 86400 = 1 day). Currently inert — see the [heat] table

[activation]
propagation_factor = 0.3   # Fraction of heat passed per hop (default: 0.3)
max_hops = 2               # Maximum graph hops for spreading activation (default: 2)
top_n = 3                  # Number of top retrieval results that trigger spreading activation (default: 3)

[cluster]
join_threshold = 0.75              # Cosine similarity threshold to join existing cluster (default: 0.75)
merge_threshold = 0.9              # Centroid similarity above which two clusters merge (default: 0.9)
cohesion_floor = 0.6               # Avg member-to-centroid similarity below which a cluster splits (default: 0.6)
maintenance_interval_secs = 300    # Cluster maintenance check interval in seconds (default: 300)

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
| `max_tokens` | usize | `128` | Longest text one embedding sees, in wordpiece tokens including `[CLS]`/`[SEP]`. A longer memory is still stored whole, but only its first `max_tokens` tokens are searchable; the server logs a warning with the fact id and token count, and `store_memory` returns `truncated: true`. **128** is the standard: it is what the model's `tokenizer.json` ships, so every database created before this key existed is at 128 and boots unchanged. **256** is tested (it is what sentence-transformers serves this model at, and it covers all but the longest ~1.7% of the corpus it was measured on — that corpus's p99 is 284 tokens, so 256 falls just short of it; see [docs/minilm-test-data.md](minilm-test-data.md), "256-token re-embed"). Anything above 256 is experimental: the model was trained at 128 and nothing here has measured it. Must be between 3 and 512, checked at config load, and no larger than the model's position table, checked when the model loads. Padding is off at every value, so a text costs its own length, not the limit. |

**Switching models on an existing database:** stop the server, set the new `model`, run `alexandria migrate-embeddings` (re-embeds every memory and cluster centroid, then updates the lock), and start the server again. Thresholds (`[cluster]`, `[retrieve] min_similarity`, and the client's `[recall] min_similarity`) are tuned to the default model; retune them if you switch. `alexandria bench-retrieval` derives the latter two from the new model's own output — see [docs/minilm-test-data.md](minilm-test-data.md). The migration is not transactional: if it fails partway, rerun it. Do not revert `model` in config afterwards, the database may hold a mix of old and new vectors.

**Raising the token limit on an existing database:** stop the server, set `max_tokens`, run `alexandria migrate-embeddings`, start the server. It re-embeds everything, as a model switch does, and moves the lock last. The limit only goes up: `migrate-embeddings` refuses a `max_tokens` below what the corpus is locked at, and the server refuses to boot on one, so the way back from 256 is to keep `max_tokens = 256`. `alexandria migrate-embeddings --force` re-embeds even when the lock already matches config, for a corpus whose lock is right and whose vectors may not be.

**Model locking:** On first boot, the model name, dimension count, and token limit are stored in the database. Changing the model or the limit in config without migrating causes a startup error that says which way to go. A database locked before the token limit was recorded, or one that has memories and no lock at all, was embedded at 128 tokens and is treated as locked at 128: it boots at the default and refuses a higher `max_tokens` until `alexandria migrate-embeddings` has run.

**Rolling back the binary is unsafe after a migration, and undetectable.** A release from before `max_tokens` existed does not read the token lock. Run against a database migrated to 256, it writes 128-token, padded vectors into a corpus labelled 256, and nothing can tell afterwards. From this release on, a binary refuses to open a database whose schema version is newer than its own, which stops the general case; it cannot stop binaries that predate the check. If it has happened, `alexandria migrate-embeddings --force` repairs it.

**First run:** The model weights (~80MB for all-MiniLM-L6-v2) are downloaded from HuggingFace Hub and cached in `~/.cache/huggingface/`.

### `[heat]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `spacing_halflife_secs` | f64 | `86400.0` | **Currently inert.** Intended as the base half-life for the Ebbinghaus spaced-repetition curve, but `decay.rs:projected_heat` hardcodes `tau = stability * 86400.0` and takes no half-life, and nothing outside the engine crate calls it — the configured value is read only to display in the debug dashboard. Wiring it up or deleting it is the open A2 decision in [docs/performance-and-ability-findings.md](performance-and-ability-findings.md). The direction is also the reverse of the intuitive reading: in `decay.rs:on_access` a *lower* value raises the spacing ratio, which grows stability faster and therefore cools *slower*. |

### `[activation]`

Controls spreading activation — when a memory is accessed, its graph neighbors receive a fraction of heat.

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `propagation_factor` | f32 | `0.3` | Heat fraction passed per hop. At hop 1, a neighbor gets `propagation_factor × edge_strength` of the source heat. At hop 2, `propagation_factor² × edge_strength`. |
| `max_hops` | u32 | `2` | Maximum graph traversal depth. Higher values spread activation further but cost more DB queries. |
| `top_n` | integer | `3` | Number of top retrieval results that trigger spreading activation. Only the top N results from `retrieve_memories` fire the activation side effect. |

### `[cluster]`

Controls automatic cluster assignment, splitting, and merging. Maintenance runs periodically in HTTP mode (controlled by `maintenance_interval_secs`).

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `join_threshold` | f32 | `0.75` | Minimum cosine similarity between a new memory's embedding and a cluster centroid to join that cluster. Below this, a new cluster is created. |
| `merge_threshold` | f32 | `0.9` | Centroid-to-centroid similarity above which two clusters are merged. |
| `cohesion_floor` | f32 | `0.6` | Average member-to-centroid similarity below which a cluster is split via k-means(k=2). |
| `maintenance_interval_secs` | u64 | `300` | Interval between cluster maintenance runs in seconds (default: 5 minutes). Only active in HTTP mode. |

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
