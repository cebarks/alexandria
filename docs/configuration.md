# Configuration Reference

Alexandria loads server config with this precedence:

1. **Built-in defaults**
2. **Config file** — exactly one file is loaded, chosen by first-match priority:
   - `ALEXANDRIA_CONFIG` env var (explicit path override)
   - `$XDG_CONFIG_HOME/alexandria/config.toml` (default: `~/.config/alexandria/config.toml` on Linux, `~/Library/Application Support/alexandria/config.toml` on macOS)
   - `~/.alexandria/config.toml` (legacy fallback, logged with a warning)
3. **Individual env vars** — `ALEXANDRIA_SERVER_TRANSPORT`, `ALEXANDRIA_SERVER_HOST`, `ALEXANDRIA_SERVER_PORT`, `ALEXANDRIA_DATA_DIR`, `ALEXANDRIA_EMBEDDING_MODEL`, `ALEXANDRIA_EMBEDDING_DEVICE`, `ALEXANDRIA_EMBEDDING_BATCH_SIZE`, `ALEXANDRIA_REMINDERS_TIMEZONE`, `ALEXANDRIA_REMINDERS_ESCALATION_HOURS`

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
spacing_halflife_secs = 86400.0   # Spaced repetition half-life in seconds (default: 86400 = 1 day)

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
| `batch_size` | usize | `32` | Facts per `embed()` call during `alexandria migrate-embeddings`. Bounds peak memory on large corpora; must be at least 1, checked at config load. The server itself embeds one text at a time. |

**Switching models on an existing database:** stop the server, set the new `model`, run `alexandria migrate-embeddings` (re-embeds every memory and cluster centroid, then updates the lock), and start the server again. Thresholds (`[cluster]` and `[retrieve] min_similarity`) are tuned to the default model; retune them if you switch. The migration is not transactional: if it fails partway, rerun it. Do not revert `model` in config afterwards, the database may hold a mix of old and new vectors.

**Model locking:** On first boot, the model name and dimension count are stored in the database. Changing the model in config without wiping the database will cause a startup error with instructions to either revert the model or run `alexandria migrate-embeddings`.

**First run:** The model weights (~80MB for all-MiniLM-L6-v2) are downloaded from HuggingFace Hub and cached in `~/.cache/huggingface/`.

### `[heat]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `spacing_halflife_secs` | f64 | `86400.0` | Base half-life for the Ebbinghaus spaced repetition curve, in seconds. Lower values mean memories cool faster without re-access. |

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
| `min_similarity` | f32 | `0.10` | Hard floor on cosine similarity below which results are dropped, regardless of the requested `limit`. A noise cutoff only. Model-dependent: for `all-MiniLM-L6-v2` (measured 2026-09-08), a keyword or near-paraphrase hit scores 0.55–0.76, a natural-language question against its matching statement 0.40–0.65, and a question sharing no vocabulary with the statement as low as ~0.2. Unrelated memories score 0.07–0.40. The floor stays below the vocabulary-free cases; client thresholds do the real filtering. Derived by the retrieve-floor rule in `docs/plans/2026-09-08-embedding-model-swap-design.md` (median non-hit score, rounded to two decimals, checked to sit below the lowest correct hit); the rule gives 0.08 for MiniLM and 0.10 is kept because the difference is immaterial. |

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
limit = 5
min_similarity = 0.58

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
| `limit` | number | `5` | `ALEXANDRIA_AUTO_RECALL_LIMIT` | Max memories to retrieve per prompt. |
| `min_similarity` | number | `0.58` | `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | Minimum cosine similarity to include an auto-recalled memory. **Recommended: `0.35`.** The `0.58` Pi default predates measurement and assumed genuine matches score 0.6+; on `all-MiniLM-L6-v2` (measured 2026-09-08, synthetic pairs) question-vs-matching-statement scores 0.40–0.65 and unrelated memories 0.07–0.40, so `0.58` drops most real hits. `0.35` keeps them and admits only topically adjacent memories. The Claude Code hook (`contrib/claude`) already defaults to `0.35`; the Pi default is left at `0.58` pending a change to the extension. |

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
