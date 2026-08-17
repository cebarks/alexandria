# Alexandria

Agent memory server with tiered maturity, hierarchical clustering, spreading activation, and progressive recall. Runs as a persistent [MCP](https://modelcontextprotocol.io/) service backed by embedded [SurrealDB](https://surrealdb.com/).

## Features

- **Semantic search** — Cosine similarity over local embeddings (all-MiniLM-L6-v2 via candle, pure Rust)
- **Ebbinghaus heat model** — Memories have heat (recency) and stability (spaced repetition). Frequently accessed memories stay hot; forgotten ones cool.
- **Spreading activation** — Accessing a memory warms its graph neighbors. Heat propagates along edges with configurable decay.
- **Graph edges** — Memories link via `relates_to`, `supports`, `contradicts`, `derived_from`, and `extracted_from` edges
- **Hierarchical clustering** — Automatic cluster assignment on store, background split/merge maintenance
- **Progressive recall** — Two-phase retrieval: broad cluster matching first, then scope-narrowing within a cluster
- **Document import** — Chunk by heading, paragraph, or fixed size with batch tracking and `extracted_from` lineage
- **Persistent storage** — SurrealKV on disk, survives restarts
- **Schema migrations** — Versioned `.surql` files with forward-only migration runner

## Quick Start

```bash
# Build and install
cargo install --path crates/alexandria

# Create config
mkdir -p ~/.alexandria
cat > ~/.alexandria/config.toml << 'EOF'
[server]
transport = "http"
port = 3000

[embedding]
model = "sentence-transformers/all-MiniLM-L6-v2"
device = "cpu"
EOF

# Run
alexandria
```

First run downloads the embedding model from HuggingFace Hub (~80MB).

## MCP Tools

| Tool | Description |
| ------ | ------------- |
| `store_memory` | Store text with auto-embedding, clustering, and heat initialization |
| `retrieve_memories` | Semantic similarity search with spreading activation on top results |
| `recall` | Progressive two-phase recall: broad cluster scan → focused scope narrowing |
| `update_memory` | Update content/tags/confidence; content changes re-embed and create lineage |
| `import_document` | Import and chunk documents with `extracted_from` edge tracking |
| `delete_memory` | Soft-delete a memory by ID |

## Deployment

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
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now alexandria
journalctl --user -u alexandria -f  # tail logs
```

### MCP client configuration

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

## Configuration

Config loads with precedence: defaults → `~/.alexandria/config.toml` → `ALEXANDRIA_CONFIG` env → individual env vars.

See [docs/configuration.md](docs/configuration.md) for all options.

## Architecture

```
crates/
├── alexandria/          # Binary — config loading, transport setup, main loop
├── alexandria-mcp/      # MCP tool handlers, server struct
├── alexandria-engine/   # Core algorithms — clustering, heat, recall, import, activation
├── alexandria-pipeline/ # Embedding providers (candle)
└── alexandria-storage/  # SurrealDB connection, models, repos, schema migrations
```

Data flows: **MCP request → server handler → engine algorithm → storage repo → SurrealDB**

## Development

```bash
cargo test --workspace          # 64 tests
cargo build --workspace         # debug build
cargo clippy --workspace        # lint
RUST_LOG=debug cargo run        # run with debug logging
```

## License

MIT
