# Configuration Reference

Alexandria loads config with this precedence (last wins):

1. **Built-in defaults**
2. **`~/.alexandria/config.toml`**
3. **`ALEXANDRIA_CONFIG` env var** — path to an alternate TOML file
4. **Individual env vars** — `ALEXANDRIA_SERVER_TRANSPORT`, `ALEXANDRIA_DATABASE_DATA_DIR`, etc.

## Full Example

```toml
[server]
transport = "http"          # "stdio" or "http" (default: "stdio")
host = "127.0.0.1"          # HTTP bind address (default: "127.0.0.1")
port = 3000                 # HTTP port (default: 3000)
allowed_origins = ["*"]     # CORS origins; ["*"] disables validation (default: ["*"])
allowed_hosts = ["*"]       # Allowed Host headers; ["*"] disables validation (default: ["*"])

[database]
data_dir = "~/.alexandria/data"   # Storage path; ":memory:" for ephemeral (default: "~/.alexandria/data")

[embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"   # HuggingFace model ID (no default — required)
device = "cpu"                                       # "cpu" only for now (default: "cpu")

[heat]
spacing_halflife_secs = 86400.0   # Spaced repetition half-life in seconds (default: 86400 = 1 day)

[activation]
propagation_factor = 0.3   # Fraction of heat passed per hop (default: 0.3)
max_hops = 2               # Maximum graph hops for spreading activation (default: 2)

[cluster]
join_threshold = 0.75    # Cosine similarity threshold to join existing cluster (default: 0.75)
merge_threshold = 0.9    # Centroid similarity above which two clusters merge (default: 0.9)
cohesion_floor = 0.6     # Avg member-to-centroid similarity below which a cluster splits (default: 0.6)
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

### `[database]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `data_dir` | path | `~/.alexandria/data` | SurrealKV storage directory. Set to `":memory:"` for ephemeral in-memory storage (data lost on restart). |

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

### `[cluster]`

Controls automatic cluster assignment, splitting, and merging. Maintenance runs every 5 minutes in HTTP mode.

| Key | Type | Default | Description |
| ----- | ------ | --------- | ------------- |
| `join_threshold` | f32 | `0.75` | Minimum cosine similarity between a new memory's embedding and a cluster centroid to join that cluster. Below this, a new cluster is created. |
| `merge_threshold` | f32 | `0.9` | Centroid-to-centroid similarity above which two clusters are merged. |
| `cohesion_floor` | f32 | `0.6` | Average member-to-centroid similarity below which a cluster is split via k-means(k=2). |

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
