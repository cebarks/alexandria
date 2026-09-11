# Configuration Reference

Alexandria loads server config with this precedence:

1. **Built-in defaults**
2. **Config file** — exactly one file is loaded, chosen by first-match priority:
   - `ALEXANDRIA_CONFIG` env var (explicit path override)
   - `$XDG_CONFIG_HOME/alexandria/config.toml` (default: `~/.config/alexandria/config.toml` on Linux, `~/Library/Application Support/alexandria/config.toml` on macOS)
   - `~/.alexandria/config.toml` (legacy fallback, logged with a warning)
3. **Individual env vars** — `ALEXANDRIA_SERVER_TRANSPORT`, `ALEXANDRIA_SERVER_HOST`, `ALEXANDRIA_SERVER_PORT`, `ALEXANDRIA_DATA_DIR`, `ALEXANDRIA_EMBEDDING_MODEL`, `ALEXANDRIA_EMBEDDING_DEVICE`, `ALEXANDRIA_EMBEDDING_BATCH_SIZE`

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

**Switching models on an existing database:** stop the server, set the new `model`, run `alexandria migrate-embeddings` (re-embeds every memory and cluster centroid, then updates the lock), and start the server again. Thresholds (`[cluster]`, `[retrieve] min_similarity`, and the client's `[recall] min_similarity`) are tuned to the default model; retune them if you switch. `alexandria bench-retrieval` derives the latter two from the new model's own output — see [docs/minilm-test-data.md](minilm-test-data.md). The migration is not transactional: if it fails partway, rerun it. Do not revert `model` in config afterwards, the database may hold a mix of old and new vectors.

**Model locking:** On first boot, the model name, dimension count, and token limit (256 wordpiece tokens per text; longer texts embed on their first 256 and log a warning) are stored in the database. Changing the model in config without wiping the database will cause a startup error with instructions to either revert the model or run `alexandria migrate-embeddings`. A database locked before the token limit was recorded was embedded at 128 tokens; the server refuses to boot on it until `alexandria migrate-embeddings` re-embeds everything at 256.

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
| `min_similarity` | f32 | `0.10` | Hard floor on cosine similarity below which results are dropped, regardless of the requested `limit`. A noise cutoff only — the client's `[recall] min_similarity` does the real filtering. Derived by the retrieve-floor rule, which `alexandria bench-retrieval` computes from the model's own output: the median non-hit score rounded to two decimals, valid only if it sits below the weakest correct hit. **That result is a property of the model *and* the corpus, not of the model alone, and drifts down as the corpus grows** — it gave `0.08` at 143 facts and `0.07` at 807. `0.10` is kept regardless, because on `all-MiniLM-L6-v2` the weakest true hit scores 0.338 and every candidate floor sits far below it. Current score ranges and the full derivation are in [docs/minilm-test-data.md](minilm-test-data.md); do not restate them here. |

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

The `ALEXANDRIA_SERVER_*` variables exist so a container can be configured entirely by environment
(the bundled [Dockerfile](../Dockerfile) uses them to default to HTTP on `0.0.0.0:3000`) without
shipping a config file.

Everything else — `[heat]`, `[activation]`, `[cluster]`, `[retrieve]`, CORS, and SSE keep-alive —
can only be set via the TOML file.

---

## Client Configuration

The Pi auto-recall/auto-store extension loads its own config from `$XDG_CONFIG_HOME/alexandria/client.toml`.
It is a separate file with separate keys — the server never reads it and the extension never reads
`config.toml`.

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
```

### `[server]`

| Key | Type | Default | Env Override | Description |
|-----|------|---------|-------------|-------------|
| `url` | string | `"http://127.0.0.1:3000/mcp"` | `ALEXANDRIA_URL` | Alexandria MCP server endpoint URL. |

### `[recall]`

| Key | Type | Default | Env Override | Description |
| ----- | ------ | --------- | ------------- | ------------- |
| `enabled` | bool | `true` | `ALEXANDRIA_AUTO_RECALL=off` | Enable auto-recall on every prompt. |
| `limit` | number | `10` | `ALEXANDRIA_AUTO_RECALL_LIMIT` | Max memories to retrieve per prompt. It is not merely a cap — a target ranked below it cannot be surfaced by any threshold, so it is a recall lever in its own right, and the stronger of the two. Measured 2026-09-09 on an 880-fact corpus by `alexandria bench-retrieval`'s limit × threshold grid (see [docs/minilm-test-data.md](minilm-test-data.md), "Result limit"): delivery saturates at `10`, because the worst of the 12 benchmark target ranks is 9, so `15` and `20` add non-target memories and no hits. Was `5` until that measurement, which hid 3 of the 11 targets that cleared the then-default threshold on score. Lowering it below `3` also silently narrows spreading activation (`activation.top_n`). |
| `min_similarity` | number | `0.45` | `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | Minimum cosine similarity to include an auto-recalled memory. Measured by the same grid, which counts how many of 12 known targets a threshold actually delivers *through* `limit`. **Read it together with `limit` — the two are not independent.** At `limit = 10`: `0.45` delivers 8/12 at ~1.0 non-targets per prompt, `0.35` delivers 11/12 at ~4.7, `0.50` delivers 7/12 at ~0.5, and the old `0.58` Pi default delivers only 4/12 — it assumed genuine matches score 0.6+ and drops two thirds of real hits. `0.40` was dominated on that 12-question grid (same 8 delivered as `0.45`, roughly double the noise); with the 20-question set it delivers 16/20 at ~3.0 against `0.45`'s 14/20 at ~1.65, a genuine trade that the strict-dominance rule used to pick the pair does not take. `0.45` is chosen as the frontier pick: paired with `limit = 10` it delivers what the previous `5`/`0.35` pair did at a third of the injection. Note that `0.45` is only on the frontier *because* the limit is 10 — at `limit = 5` it was dominated by `0.50`, so do not lower one without revisiting the other. It is not free: the weakest target scores 0.338 and falls below it. |

### `[store]`

| Key | Type | Default | Env Override | Description |
| ----- | ------ | --------- | ------------- | ------------- |
| `enabled` | bool | `true` | `ALEXANDRIA_AUTO_STORE=off` | Enable heuristic store detectors and LLM extraction. |
| `extract_model` | string | `"vertex/claude-haiku-4-5"` | `ALEXANDRIA_EXTRACT_MODEL` | Model for session-end LLM extraction. Falls back to session model if unavailable. |
| `extract_timeout_ms` | number | `5000` | `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | Timeout for the extraction LLM call in milliseconds. |

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
