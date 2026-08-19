# Configuration Reference

Alexandria loads server config with this precedence (last wins):

1. **Built-in defaults**
2. **`$XDG_CONFIG_HOME/alexandria/config.toml`** (default: `~/.config/alexandria/config.toml` on Linux, `~/Library/Application Support/alexandria/config.toml` on macOS)
3. **`~/.alexandria/config.toml`** — legacy fallback, used only if the XDG path doesn't exist
4. **`ALEXANDRIA_CONFIG` env var** — path to an alternate TOML file
5. **Individual env vars** — `ALEXANDRIA_SERVER_TRANSPORT`, `ALEXANDRIA_DATABASE_DATA_DIR`, etc.

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
data_dir = "~/.local/share/alexandria/data"   # Storage path; ":memory:" for ephemeral (default: $XDG_DATA_HOME/alexandria/data)

[embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"   # HuggingFace model ID (no default — required)
device = "cpu"                                       # "cpu" only for now (default: "cpu")

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
| `model` | string | `"sentence-transformers/all-MiniLM-L6-v2"` | HuggingFace model ID. Must be a BERT-family model compatible with candle. |
| `device` | string | `"cpu"` | Compute device. Only `"cpu"` is currently supported. |

**Model locking:** On first boot, the model name and dimension count are stored in the database. Changing the model in config without wiping the database will cause a startup error with instructions to either revert the model or run a migration.

**First run:** The model weights (~80MB for all-MiniLM-L6-v2) are downloaded from HuggingFace Hub and cached in `~/.cache/huggingface/`.

### `[heat]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `spacing_halflife_secs` | f64 | `86400.0` | Base half-life for the Ebbinghaus spaced repetition curve, in seconds. Lower values mean memories cool faster without re-access. |

### `[activation]`

Controls spreading activation — when a memory is accessed, its graph neighbors receive a fraction of heat.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `propagation_factor` | f32 | `0.3` | Heat fraction passed per hop. At hop 1, a neighbor gets `propagation_factor × edge_strength` of the source heat. At hop 2, `propagation_factor² × edge_strength`. |
| `max_hops` | u32 | `2` | Maximum graph traversal depth. Higher values spread activation further but cost more DB queries. |
| `top_n` | usize | `3` | Number of top retrieval results that trigger spreading activation. Only the top N results from `retrieve_memories` fire the activation side effect. |

### `[cluster]`

Controls automatic cluster assignment, splitting, and merging. Maintenance runs periodically in HTTP mode (controlled by `maintenance_interval_secs`).

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `join_threshold` | f32 | `0.75` | Minimum cosine similarity between a new memory's embedding and a cluster centroid to join that cluster. Below this, a new cluster is created. |
| `merge_threshold` | f32 | `0.9` | Centroid-to-centroid similarity above which two clusters are merged. |
| `cohesion_floor` | f32 | `0.6` | Average member-to-centroid similarity below which a cluster is split via k-means(k=2). |
| `maintenance_interval_secs` | u64 | `300` | Interval between cluster maintenance runs in seconds (default: 5 minutes). Only active in HTTP mode. |

## Environment Variable Override

Any config key can be overridden via environment variable using the pattern `ALEXANDRIA_{SECTION}_{KEY}` in uppercase:

```bash
ALEXANDRIA_SERVER_TRANSPORT=http
ALEXANDRIA_SERVER_PORT=8080
ALEXANDRIA_DATABASE_DATA_DIR=/var/lib/alexandria
ALEXANDRIA_EMBEDDING_MODEL=sentence-transformers/all-MiniLM-L6-v2
ALEXANDRIA_HEAT_SPACING_HALFLIFE_SECS=172800
ALEXANDRIA_ACTIVATION_PROPAGATION_FACTOR=0.5
ALEXANDRIA_CLUSTER_JOIN_THRESHOLD=0.8
```

---

## Client Configuration

The Pi auto-recall/store extension loads its own config from `$XDG_CONFIG_HOME/alexandria/client.toml`.

Precedence: defaults → `client.toml` → `ALEXANDRIA_CLIENT_CONFIG` env var (path to alt TOML) → individual `ALEXANDRIA_*` env vars.

### Full Example

```toml
[server]
url = "http://127.0.0.1:3000/mcp"

[recall]
enabled = true
limit = 5
min_similarity = 0.5

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
|-----|------|---------|-------------|-------------|
| `enabled` | bool | `true` | `ALEXANDRIA_AUTO_RECALL=off` | Enable auto-recall on every prompt. |
| `limit` | number | `5` | `ALEXANDRIA_AUTO_RECALL_LIMIT` | Max memories to retrieve per prompt. |
| `min_similarity` | number | `0.5` | `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | Minimum cosine similarity to include a result. |

### `[store]`

| Key | Type | Default | Env Override | Description |
|-----|------|---------|-------------|-------------|
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
