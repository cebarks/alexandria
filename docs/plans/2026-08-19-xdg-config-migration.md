# XDG Config Migration Implementation Plan

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Migrate Alexandria server and client config/data paths from `~/.alexandria/` to XDG Base Directory spec, add a `client.toml` config file for the Pi extension, and add new server config options.

**Architecture:** Server config moves to `$XDG_CONFIG_HOME/alexandria/config.toml`, data to `$XDG_DATA_HOME/alexandria/data`. A new `$XDG_CONFIG_HOME/alexandria/client.toml` configures the Pi extension. Both support env var overrides. The server falls back to `~/.alexandria/` if XDG paths don't exist yet (migration path). New server config options: `server.sse_keep_alive_secs`, `cluster.maintenance_interval_secs`, `activation.top_n`.

**Tech Stack:** Rust (`dirs` crate for XDG, `serial_test` for test isolation), TypeScript (TOML parser — `smol-toml`), TOML config files

---

## Task 1: Server — Add `serial_test` dev-dependency

**Files:**

- Modify: `crates/alexandria/Cargo.toml`

Existing tests in `config.rs` (`test_env_overrides`) already mutate env vars without serialization. New config path tests will add more. Since `cargo test` runs tests in parallel within a binary, these race. Add `serial_test` to serialize env-mutating tests.

**Step 1: Add `serial_test` dev-dependency**

```bash
cd ~/code/alexandria
cargo add serial_test --dev -p alexandria
```

**Step 2: Annotate existing `test_env_overrides` with `#[serial]`**

In `crates/alexandria/src/config.rs`, add `use serial_test::serial;` in the test module and annotate `test_env_overrides` with `#[serial]`.

**Step 3: Verify existing tests pass**

Run: `cargo test -p alexandria -- --nocapture`
Expected: All pass

**Step 4: Commit**

```bash
git add crates/alexandria/Cargo.toml crates/alexandria/src/config.rs
git commit -m "deps: add serial_test for env-var test isolation"
```

---

## Task 2: Server — Migrate config path to XDG

**Files:**

- Modify: `crates/alexandria/src/config.rs`

**Step 1: Write tests for XDG path resolution**

Add tests that validate the new path resolution logic. The fallback behavior is:

1. `$ALEXANDRIA_CONFIG` env var (explicit override, unchanged)
2. `$XDG_CONFIG_HOME/alexandria/config.toml` (new primary)
3. `~/.alexandria/config.toml` (legacy fallback)

All env-mutating tests must use `#[serial]`.

```rust
#[test]
#[serial]
fn test_xdg_config_path_default() {
    // When no env override, default should use XDG config dir
    std::env::remove_var("ALEXANDRIA_CONFIG");
    let path = config_path();
    // Should end with alexandria/config.toml under XDG_CONFIG_HOME or ~/.config
    assert!(path.ends_with("alexandria/config.toml"));
    assert!(!path.starts_with(
        dirs::home_dir().unwrap().join(".alexandria")
    ));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p alexandria test_xdg_config_path -- --nocapture`
Expected: FAIL — `config_path()` doesn't exist yet

**Step 3: Implement XDG config path resolution**

Replace the path logic in `Config::load()`. Extract a `config_path()` helper:

```rust
/// Resolve the config file path with precedence:
/// 1. ALEXANDRIA_CONFIG env var
/// 2. $XDG_CONFIG_HOME/alexandria/config.toml
/// 3. ~/.alexandria/config.toml (legacy fallback)
fn config_path() -> PathBuf {
    // Explicit env override wins
    if let Ok(p) = std::env::var("ALEXANDRIA_CONFIG") {
        return PathBuf::from(p);
    }

    // XDG primary
    let xdg_path = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("alexandria")
        .join("config.toml");
    if xdg_path.exists() {
        return xdg_path;
    }

    // Legacy fallback
    let legacy_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".alexandria")
        .join("config.toml");
    if legacy_path.exists() {
        tracing::warn!(
            "Using legacy config path {}. Consider moving to {}",
            legacy_path.display(),
            xdg_path.display(),
        );
        return legacy_path;
    }

    // Neither exists — prefer XDG for new installs
    xdg_path
}
```

Update `Config::load()` to call `config_path()` instead of inline logic.

**Step 4: Run test to verify it passes**

Run: `cargo test -p alexandria test_xdg_config_path -- --nocapture`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/alexandria/src/config.rs
git commit -m "refactor(config): migrate config path to XDG_CONFIG_HOME with legacy fallback"
```

---

## Task 3: Server — Migrate data path default to XDG

**Files:**

- Modify: `crates/alexandria/src/config.rs`

**Step 1: Write test for XDG data path default**

```rust
#[test]
fn test_xdg_data_dir_default() {
    let config = Config::default();
    // Default data dir should be under XDG_DATA_HOME, not ~/.alexandria
    let xdg_data = dirs::data_dir().unwrap().join("alexandria").join("data");
    assert_eq!(config.database.data_dir, xdg_data);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p alexandria test_xdg_data_dir -- --nocapture`
Expected: FAIL — current default is `~/.alexandria/data`

**Step 3: Update `default_data_dir()` to use XDG**

```rust
fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        })
        .join("alexandria")
        .join("data")
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p alexandria test_xdg_data_dir -- --nocapture`
Expected: PASS

**Step 5: Fix the existing `test_defaults` test**

The existing test `assert!(config.database.data_dir.ends_with("data"))` should still pass, but verify and update if needed.

Run: `cargo test -p alexandria test_defaults -- --nocapture`
Expected: PASS (the assertion only checks `ends_with("data")`)

**Step 6: Commit**

```bash
git add crates/alexandria/src/config.rs
git commit -m "refactor(config): migrate default data_dir to XDG_DATA_HOME"
```

---

## Task 4: Server — Add new configurable options

**Files:**

- Modify: `crates/alexandria/src/config.rs`
- Modify: `crates/alexandria/src/main.rs`
- Modify: `crates/alexandria-mcp/src/server.rs`

**Step 1: Write test for new config fields with defaults**

```rust
#[test]
fn test_new_config_defaults() {
    let config = Config::default();
    assert_eq!(config.server.sse_keep_alive_secs, 15);
    assert_eq!(config.cluster.maintenance_interval_secs, 300);
    assert_eq!(config.activation.top_n, 3);
}

#[test]
fn test_new_config_from_toml() {
    let toml = r#"
        [server]
        sse_keep_alive_secs = 30

        [cluster]
        maintenance_interval_secs = 600

        [activation]
        top_n = 5
    "#;
    let config = Config::from_toml(toml).unwrap();
    assert_eq!(config.server.sse_keep_alive_secs, 30);
    assert_eq!(config.cluster.maintenance_interval_secs, 600);
    assert_eq!(config.activation.top_n, 5);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p alexandria test_new_config -- --nocapture`
Expected: FAIL — fields don't exist

**Step 3: Add fields to config structs**

In `ServerConfig`:

```rust
/// SSE keep-alive interval in seconds. Default: 15.
pub sse_keep_alive_secs: u64,
```

Default: `sse_keep_alive_secs: 15`

In `ClusterConfig`:

```rust
/// Cluster maintenance check interval in seconds. Default: 300 (5 minutes).
pub maintenance_interval_secs: u64,
```

Default: `maintenance_interval_secs: 300`

In config.rs's `ActivationConfig` (NOT `alexandria-engine`'s — that struct controls heat propagation only):

```rust
/// Number of top retrieval results that trigger spreading activation. Default: 3.
pub top_n: usize,
```

Default: `top_n: 3`

Note: there are two `ActivationConfig` structs — one in `config.rs` (for TOML deserialization) and one in `alexandria-engine::heat::activation` (for the propagation algorithm). `top_n` belongs only in config.rs's version. The engine struct stays untouched.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p alexandria test_new_config -- --nocapture`
Expected: PASS

**Step 5: Wire new config values into `main.rs`**

Replace hardcoded values in `main.rs`:

- `Duration::from_secs(15)` → `Duration::from_secs(config.server.sse_keep_alive_secs)`
- `Duration::from_secs(300)` → `Duration::from_secs(config.cluster.maintenance_interval_secs)`

**Step 6: Wire `activation.top_n` through `AlexandriaServer`**

Add `activation_top_n: usize` as a separate field on `AlexandriaServer` (like `cluster_join_threshold`). Do NOT add it to the engine's `ActivationConfig`.

In `main.rs`, pass it during construction:

```rust
let server = AlexandriaServer::new(
    Arc::new(db),
    Arc::new(embedding),
    config.cluster.join_threshold,
    config.heat.spacing_halflife_secs,
)
.with_activation_config(activation_config)
.with_activation_top_n(config.activation.top_n);
```

In `crates/alexandria-mcp/src/server.rs`, add the field and builder method:

```rust
pub struct AlexandriaServer {
    // ...existing fields...
    activation_top_n: usize,
}

impl AlexandriaServer {
    pub fn with_activation_top_n(mut self, n: usize) -> Self {
        self.activation_top_n = n;
        self
    }
}
```

Default `activation_top_n` to `3` in `AlexandriaServer::new()`.

Replace the hardcoded `.take(3)` on line 350 of `server.rs`:

```rust
// Before:
for (idx, _) in ranked.iter().take(3) {
// After:
for (idx, _) in ranked.iter().take(self.activation_top_n) {
```

**Step 7: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass

**Step 8: Commit**

```bash
git add crates/alexandria/src/config.rs crates/alexandria/src/main.rs crates/alexandria-mcp/src/server.rs
git commit -m "feat(config): add sse_keep_alive_secs, maintenance_interval_secs, activation.top_n"
```

---

## Task 5: Server — Existing data migration helper

**Files:**

- Modify: `crates/alexandria/src/main.rs`

This isn't automatic migration — just a startup log message that tells the user what to do.

**Step 1: Add startup migration check to `main.rs`**

After `Config::load()`, before connecting to DB:

```rust
// Check for legacy data dir and advise migration
let legacy_data = dirs::home_dir()
    .unwrap_or_else(|| PathBuf::from("."))
    .join(".alexandria")
    .join("data");
if legacy_data.exists() && config.database.data_dir != legacy_data {
    tracing::warn!(
        "Legacy data directory found at {}. To migrate, run:\n  \
         mv {} {}",
        legacy_data.display(),
        legacy_data.display(),
        config.database.data_dir.display(),
    );
}
```

**Step 2: Verify it compiles and existing tests pass**

Run: `cargo test --workspace`
Expected: All pass

**Step 3: Commit**

```bash
git add crates/alexandria/src/main.rs
git commit -m "feat: add startup warning for legacy data dir migration"
```

---

## Task 6: Client — Add TOML parser dependency

**Files:**

- Modify: `contrib/pi/extensions/alexandria-auto-recall/package.json`

**Step 1: Add `smol-toml` dependency**

`smol-toml` is a small, spec-compliant TOML parser with zero dependencies. It's the right choice over `@iarna/toml` (unmaintained) or `toml` (Node.js native bindings).

```bash
cd contrib/pi/extensions/alexandria-auto-recall
npm install smol-toml
```

**Step 2: Verify install**

Run: `cd contrib/pi/extensions/alexandria-auto-recall && node --input-type=module -e "import('smol-toml')"`
Expected: No error (the extension is `type: module` — use ESM import, not `require()`)

**Step 3: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/package.json contrib/pi/extensions/alexandria-auto-recall/package-lock.json
git commit -m "deps(extension): add smol-toml for client config file support"
```

---

## Task 7: Client — Implement config file loading

**Files:**

- Modify: `contrib/pi/extensions/alexandria-auto-recall/src/config.ts`

**Step 1: Implement TOML config file loading with env var override**

The config file lives at `$XDG_CONFIG_HOME/alexandria/client.toml` (default `~/.config/alexandria/client.toml`). Override with `ALEXANDRIA_CLIENT_CONFIG` env var. Env vars override individual values.

Precedence per-value: compiled default → TOML file → env var

```typescript
import { parse } from "smol-toml";
import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";
import { homedir, platform } from "node:os";

interface ClientToml {
  server?: { url?: string };
  recall?: { enabled?: boolean; limit?: number; min_similarity?: number };
  store?: {
    enabled?: boolean;
    extract_model?: string;
    extract_timeout_ms?: number;
  };
}

/**
 * Platform-aware config directory, matching the Rust `dirs::config_dir()` behavior:
 * - Linux:  $XDG_CONFIG_HOME or ~/.config
 * - macOS:  ~/Library/Application Support
 * - Windows: %APPDATA% (not expected, but handled)
 */
function configDir(): string {
  if (process.env.XDG_CONFIG_HOME) return process.env.XDG_CONFIG_HOME;
  const home = homedir();
  switch (platform()) {
    case "darwin":
      return join(home, "Library", "Application Support");
    case "win32":
      return process.env.APPDATA ?? join(home, "AppData", "Roaming");
    default:
      return join(home, ".config");
  }
}

function loadToml(): ClientToml {
  const configPath =
    process.env.ALEXANDRIA_CLIENT_CONFIG ??
    join(configDir(), "alexandria", "client.toml");

  if (!existsSync(configPath)) return {};

  try {
    const raw = readFileSync(configPath, "utf-8");
    return parse(raw) as ClientToml;
  } catch (err) {
    console.warn(
      `Alexandria: failed to parse ${configPath}: ${err instanceof Error ? err.message : String(err)}; using defaults`,
    );
    return {};
  }
}

const toml = loadToml();

/** Centralized configuration — TOML file with env var overrides. */
export const CONFIG = {
  serverUrl:
    process.env.ALEXANDRIA_URL ??
    toml.server?.url ??
    "http://127.0.0.1:3000/mcp",

  recallDisabled:
    process.env.ALEXANDRIA_AUTO_RECALL === "off" ||
    (toml.recall?.enabled === false &&
      process.env.ALEXANDRIA_AUTO_RECALL === undefined),

  recallLimit: Number(
    process.env.ALEXANDRIA_AUTO_RECALL_LIMIT ??
      toml.recall?.limit ??
      5,
  ),

  recallMinSimilarity: Number(
    process.env.ALEXANDRIA_AUTO_RECALL_MIN_SIMILARITY ??
      toml.recall?.min_similarity ??
      0.5,
  ),

  storeDisabled:
    process.env.ALEXANDRIA_AUTO_STORE === "off" ||
    (toml.store?.enabled === false &&
      process.env.ALEXANDRIA_AUTO_STORE === undefined),

  extractModel:
    process.env.ALEXANDRIA_EXTRACT_MODEL ??
    toml.store?.extract_model ??
    "vertex/claude-haiku-4-5",

  extractTimeoutMs: Number(
    process.env.ALEXANDRIA_EXTRACT_TIMEOUT_MS ??
      toml.store?.extract_timeout_ms ??
      5000,
  ),
} as const;
```

**Step 2: Verify TypeScript compiles**

Run: `cd contrib/pi/extensions/alexandria-auto-recall && npx tsc --noEmit`
Expected: No errors

**Step 3: Commit**

```bash
git add contrib/pi/extensions/alexandria-auto-recall/src/config.ts
git commit -m "feat(extension): load client config from XDG client.toml with env var overrides"
```

---

## Task 8: Documentation — Update config docs and example files

**Files:**

- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `docs/configuration.md`
- Create: `contrib/pi/extensions/alexandria-auto-recall/README.md` (or update if exists)

**Step 1: Update README.md config section**

Update the config precedence documentation to reflect XDG paths:

```markdown
## Configuration

### Server (`$XDG_CONFIG_HOME/alexandria/config.toml`)

defaults → `$XDG_CONFIG_HOME/alexandria/config.toml` → `ALEXANDRIA_CONFIG` env var → individual env vars

Legacy path `~/.alexandria/config.toml` is used as fallback if XDG path doesn't exist.

### Client (`$XDG_CONFIG_HOME/alexandria/client.toml`)

defaults → `$XDG_CONFIG_HOME/alexandria/client.toml` → `ALEXANDRIA_CLIENT_CONFIG` env var → individual env vars
```

Include an example `client.toml`:

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

Document the new server config options:

```toml
[server]
sse_keep_alive_secs = 15

[cluster]
maintenance_interval_secs = 300

[activation]
top_n = 3
```

**Step 2: Update `docs/configuration.md`**

This file has 3 references to `~/.alexandria/` paths:

- The main config file path
- The `data_dir` default value
- The full example config

Update all references to use XDG paths. Update the config reference table to show `$XDG_DATA_HOME/alexandria/data` as the default `data_dir`. Add documentation for new config options (`sse_keep_alive_secs`, `maintenance_interval_secs`, `top_n`). Add a "Legacy Migration" section explaining the fallback behavior.

**Step 3: Update CLAUDE.md config precedence section**

Update the "Config Precedence" section to match:

```
### Server
defaults → `$XDG_CONFIG_HOME/alexandria/config.toml` → `ALEXANDRIA_CONFIG` env var → individual env vars

### Client (Pi extension)
defaults → `$XDG_CONFIG_HOME/alexandria/client.toml` → `ALEXANDRIA_CLIENT_CONFIG` env var → individual env vars
```

**Step 4: Commit**

```bash
git add README.md CLAUDE.md docs/configuration.md
git commit -m "docs: update config documentation for XDG migration and client.toml"
```

---

## Task 9: Migrate your live config

This is a manual step — move your existing config and data to XDG paths.

**Step 1: Create XDG directories**

```bash
mkdir -p ~/.config/alexandria
mkdir -p ~/.local/share/alexandria
```

**Step 2: Move config**

```bash
cp ~/.alexandria/config.toml ~/.config/alexandria/config.toml
```

**Step 3: Move data (requires stopping Alexandria first)**

```bash
# Stop Alexandria
systemctl --user stop alexandria  # or kill the process

# Move data
mv ~/.alexandria/data ~/.local/share/alexandria/data

# Update config.toml to remove explicit data_dir (XDG default will be used)
# Or update data_dir to new path if you want to keep it explicit

# Restart Alexandria
systemctl --user start alexandria  # or however you start it
```

**Step 4: Create client.toml**

```bash
cat > ~/.config/alexandria/client.toml << 'EOF'
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
EOF
```

**Step 5: Verify old path is no longer needed**

Once confirmed working, optionally remove `~/.alexandria/` (keep a backup first).

**Step 6: Rebuild and restart Alexandria**

```bash
cd ~/code/alexandria
cargo build --release
# Restart the service
```
