# Alexandria

Agent memory server with tiered maturity, hierarchical clustering, spreading activation, and progressive recall. Runs as a persistent [MCP](https://modelcontextprotocol.io/) service backed by embedded [SurrealDB](https://surrealdb.com/).

## Features

- **Semantic search** — Cosine similarity over local embeddings (all-MiniLM-L6-v2 via candle, pure Rust)
- **Ebbinghaus heat model** — Memories have heat (recency) and stability (spaced repetition). Frequently accessed memories stay hot; forgotten ones cool.
- **Spreading activation** — Accessing a memory warms its graph neighbors. Heat propagates along edges with configurable decay.
- **Graph edges** — Memories link via `relates_to`, `supports`, `contradicts`, `derived_from`, and `extracted_from` edges
- **Hierarchical clustering** — Automatic cluster assignment on store, background split/merge maintenance with a queryable audit log
- **Progressive recall** — Two-phase retrieval: broad cluster matching first, then scope-narrowing within a cluster
- **Session memory** — Group memories by conversation, search within a session, and close it out with a summary
- **Document import** — Chunk by heading, paragraph, or fixed size with batch tracking and `extracted_from` lineage
- **Persistent storage** — SurrealKV on disk, survives restarts
- **Debug web UI** — Read-only browser for memories, clusters, graph neighborhoods, cluster maintenance history, and a live query tester
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
EOF

# Run
alexandria
```

First run downloads the embedding model from HuggingFace Hub (~80MB).

## MCP Tools

| Tool | Description |
| ------ | ------------- |
| `store_memory` | Store text with auto-embedding, clustering, and heat initialization; optional `session_id` to group it |
| `retrieve_memories` | Semantic similarity search with spreading activation on top results; optional `session_id` to scope |
| `recall` | Progressive two-phase recall: broad cluster scan → focused scope narrowing |
| `update_memory` | Update content/tags/confidence; content changes re-embed and create lineage |
| `import_document` | Import and chunk documents with `extracted_from` edge tracking |
| `delete_memory` | Soft-delete a memory by ID |
| `get_session` | Return a session's metadata plus every memory stored during it |
| `finalize_session` | Close a session with a summary, tags, and `ended_at` |

Tool and parameter descriptions are written directively ("call this proactively when…") because
that measurably changes how often client LLMs reach for them unprompted.

### Session memory

Passing `session_id` to `store_memory` groups memories under a caller-chosen handle, and the session
is created on first use. `retrieve_memories` with `session_id` restricts ranking to that session's
memories, `get_session` reads the whole session back, and `finalize_session` records its summary.
See [docs/session-memory.md](docs/session-memory.md) for the data model and current limitations.

### Getting agents to actually use memory

A memory server is only useful if agents reach for it unprompted. Alexandria nudges this at
three levels:

1. **MCP `instructions`** — the server advertises usage guidance (when to read vs. write memory)
   in its `initialize` response via `ServerInfo.instructions`. Any MCP-compliant client can surface
   this to the model. Tool descriptions are also written directively ("call this proactively
   whenever...") rather than just describing mechanics.
2. **Client-side skill** — [`contrib/pi/skills/alexandria-memory/`](contrib/pi/skills/alexandria-memory/)
   (Pi) and [`contrib/claude/skills/alexandria-memory/`](contrib/claude/skills/alexandria-memory/)
   (Claude Code) document concrete trigger conditions and tool choice guidance, mirroring how other high-usage
   MCP tools ship skills alongside themselves.
3. **Optional pi extension** — [`contrib/pi/extensions/alexandria-auto-recall/`](contrib/pi/extensions/alexandria-auto-recall/)
   runs in both directions. On `before_agent_start` it calls `retrieve_memories` for every prompt and
   injects hits above a similarity threshold, so the agent never has to decide to check memory. On the
   write side it adds heuristic detectors (corrections, stated preferences, error→resolution pairs)
   that store without being asked, plus an LLM extraction pass at `session_shutdown` for durable facts
   both the agent and the heuristics missed. This trades latency and potential noise for guaranteed
   recall and much denser capture. The Claude Code equivalent is the set of `UserPromptSubmit`,
   `PreToolUse`, and `Stop` hooks at
   [`contrib/claude/hooks/`](contrib/claude/hooks/).

Items 2 and 3 are client-side integrations, not part of the MCP server itself — see
[`contrib/pi/README.md`](contrib/pi/README.md) and [`contrib/claude/README.md`](contrib/claude/README.md)
for what they are and how to install them.

## Debug Web UI

When `transport = "http"`, a read-only debug web UI is served alongside the MCP endpoint at
`http://<host>:<port>/debug`:

- **Dashboard** (`/debug`) — live counts of facts, clusters, edges, and raw documents
- **Memories** (`/debug/memories`) — paginated search/filter of facts by content and tag; click through to a
  detail view showing heat, stability, timestamps, cluster membership, and graph edges
- **Clusters** (`/debug/clusters`) — cluster list with live member counts and cohesion; drill into
  member facts
- **Graph** (`/debug/graph/:id`) — visualizes a memory's local edge neighborhood
- **Maintenance log** (`/debug/maintenance`) — paginated history of every background cluster split and
  merge: source cluster, resulting clusters, and members moved. The only way to audit *why* the
  clustering changed since you last looked.
- **Query Tester** (`/debug/query`) — run `retrieve_memories`/`recall` live against the real
  embedding model to sanity-check retrieval quality

The debug UI has **no authentication** and is intended for a trusted network boundary (same
posture as the unauthenticated MCP endpoint) — do not expose it on a public interface without
putting a reverse proxy/auth layer in front of it.

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

### In Docker

The [`Dockerfile`](Dockerfile) at the repo root builds a musl-targeted release binary and ships it on
a minimal Alpine runtime as a non-root user (`alexandria`, uid 10001). musl is linked
*dynamically* — the build clears `crt-static` because proc-macro and `cc`-based crates misbehave with
the musl target's default static CRT. Build and run:

```bash
docker build -t alexandria .
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
image and the MCP endpoint plus debug UI are unauthenticated — publish the port only behind a
reverse proxy or on a trusted network.

### MCP client configuration

**Claude Code:**

```bash
claude mcp add --transport http --scope user alexandria http://127.0.0.1:3000/mcp
# optional: client-side skill (same as contrib/pi, with Claude Code's mcp__alexandria__<tool> names)
cp -r contrib/claude/skills/alexandria-memory ~/.claude/skills/
# optional: auto-recall hook (see contrib/claude/README.md for the settings.json snippet)
cp contrib/claude/hooks/alexandria-recall.sh ~/.claude/hooks/
```

**Generic (any MCP client):**

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

#### In pi

pi reads MCP servers from `~/.pi/agent/mcp.json` (via pi's MCP adapter extension), so a
long-running Alexandria is a one-entry change there:

```json
{
  "settings": { "toolPrefix": "server" },
  "mcpServers": {
    "alexandria": {
      "type": "http",
      "url": "http://127.0.0.1:3000/mcp",
      "lifecycle": "keep-alive"
    }
  }
}
```

`"lifecycle": "keep-alive"` is the setting that matters for an HTTP server you use constantly:
`lazy` (the adapter default) idle-disconnects after `settings.idleTimeout` minutes, and the call that
reconnects also has to re-establish Alexandria's Streamable HTTP session. `keep-alive` connects at
startup, never idle-times-out, and refreshes the tool catalog before user input — reconnecting when
the server reports the session expired, which is the client-side half of surviving an Alexandria
restart. If you run Alexandria over stdio instead (`command`), prefer `lazy-keep-alive`: each
re-spawn means loading the ~80MB Candle model, so you want the process resident after first use.

With `toolPrefix: "server"`, pi exposes the tools as `alexandria_store_memory`,
`alexandria_retrieve_memories`, and so on — the form the skill and the auto-store detectors use. The
detectors match on the tool-name *suffix*, so a different prefix convention still works.

Then optionally add the client-side nudges — see
[`contrib/pi/README.md`](contrib/pi/README.md).

## Configuration

Config loads with precedence: defaults → `$XDG_CONFIG_HOME/alexandria/config.toml` → `ALEXANDRIA_CONFIG` env → individual env vars. Data defaults to `$XDG_DATA_HOME/alexandria/data`.

Legacy `~/.alexandria/` paths are used as fallback if the XDG paths don't exist yet.

The Pi auto-recall/auto-store extension has its own config at `$XDG_CONFIG_HOME/alexandria/client.toml`.

See [docs/configuration.md](docs/configuration.md) for all options, client config reference, and migration instructions.

## Documentation

| Document | Contents |
| --- | --- |
| [docs/configuration.md](docs/configuration.md) | Every server and client config key, env overrides, XDG migration |
| [docs/session-memory.md](docs/session-memory.md) | Session data model, lifecycle, tool semantics, current limitations |
| [docs/roadmap.md](docs/roadmap.md) | Shipped milestones, known gaps, planned work |
| [contrib/pi/README.md](contrib/pi/README.md) | pi skill vs. extension: what each does, install, failure behavior |
| [AGENTS.md](AGENTS.md) | Working notes for humans and agents on this codebase — SurrealDB 3.2 gotchas, crate boundaries, task runner |
| [docs/plans/](docs/plans/) | Dated design and implementation plans for completed work (historical record, not maintained) |

## Architecture

```text
crates/
├── alexandria/          # Binary — config loading, transport setup, main loop, cluster maintenance
├── alexandria-mcp/      # MCP tool handlers + read-only debug web UI
├── alexandria-engine/   # Core algorithms — clustering, heat, recall, import, activation
├── alexandria-pipeline/ # Embedding providers (candle)
└── alexandria-storage/  # SurrealDB connection, models, repos, schema migrations

contrib/pi/              # Optional client-side pi integrations (skill + extension), not shipped
```

Data flows: **MCP request → server handler → engine algorithm → storage repo → SurrealDB**

Crate boundaries are held by convention, not tooling: `storage` owns nearly all SurrealDB access (a
few inline queries remain in `alexandria-mcp` handlers), `engine` is pure algorithms with no DB or
async, `pipeline` abstracts embedding providers behind one trait, and only `alexandria-mcp` may see
engine and storage together. See [AGENTS.md](AGENTS.md) for the full set.

## Development

`just` recipes mirror what CI runs — install [just](https://just.systems/) if you don't have it:

```bash
just          # list recipes
just test     # cargo test --all-features — 116 tests
just lint     # clippy, warnings as errors (matches CI)
just fmt-fix  # rustfmt
just ci       # fmt + lint + test + cargo-deny, the full pre-push check
just run      # run the server locally (prefix with RUST_LOG=debug for verbose logs)
```

Use a **stable** toolchain. Recent nightlies fail to build `diskann-wide` (a SurrealDB
transitive dependency) on Apple Silicon with a trait-inference error in its NEON intrinsics,
which looks like a breakage in this repo but isn't.

Install the git pre-commit hook (fmt check + clippy) once per clone:

```bash
just install-hooks
```

[`deny.toml`](deny.toml) gates licenses and known advisories via `cargo deny`, which runs as its own
CI job. [`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs fmt, clippy, tests, and
cargo-deny on push and PR.

[`.github/workflows/container.yml`](.github/workflows/container.yml) additionally builds the Docker
image and boot-tests it — waits on `/debug`, then performs a real MCP `initialize` handshake against
`/mcp` — but only when a change touches the `Dockerfile`, `.dockerignore`, the manifests, or
`crates/**`. A green `CI` run therefore says nothing about the image, and vice versa.

## License

Distributed under **AGPL-3.0-or-later** — see [`LICENSE`](LICENSE) and the `license` field in
[`Cargo.toml`](Cargo.toml). AGPL rather than MIT is deliberate: Alexandria is a long-running
network service, and the AGPL source-availability clause closes the network-service loophole that
MIT would leave open for anyone hosting a modified fork.
