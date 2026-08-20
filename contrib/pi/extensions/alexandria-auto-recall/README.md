# Alexandria Auto-Recall & Auto-Store Extension

Pi extension that automatically recalls relevant memories before each agent turn and stores durable facts at session end.

## Features

- **Auto-recall**: Queries Alexandria for memories relevant to the user's prompt and injects them into context before the agent starts.
- **Auto-store**: Heuristic detectors for corrections, preferences, and error resolutions, plus LLM extraction at session shutdown.

## Configuration

Config file: `$XDG_CONFIG_HOME/alexandria/client.toml`

Override with `ALEXANDRIA_CLIENT_CONFIG` env var, or individual `ALEXANDRIA_*` env vars.

See [docs/configuration.md](../../../../docs/configuration.md) for the full reference.

### Example `client.toml`

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

### Environment Variables

| Variable | Default | Description |
| --- | --- | --- |
| `ALEXANDRIA_URL` | `http://127.0.0.1:3000/mcp` | Alexandria server MCP endpoint |
| `ALEXANDRIA_AUTO_RECALL` | (enabled) | Set to `off` to disable auto-recall |
| `ALEXANDRIA_AUTO_RECALL_LIMIT` | `5` | Max memories to retrieve |
| `ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY` | `0.5` | Minimum cosine similarity threshold |
| `ALEXANDRIA_AUTO_STORE` | (enabled) | Set to `off` to disable all auto-store |
| `ALEXANDRIA_EXTRACT_MODEL` | `vertex/claude-haiku-4-5` | Model for LLM extraction pass |
| `ALEXANDRIA_EXTRACT_TIMEOUT_MS` | `5000` | Extraction timeout in milliseconds |
| `ALEXANDRIA_CLIENT_CONFIG` | (XDG default) | Path to alternate client TOML config |
