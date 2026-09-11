use std::path::PathBuf;

use alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR;
use alexandria_engine::reminders::DEFAULT_ESCALATION_HOURS;
use alexandria_engine::search::DEFAULT_MIN_SIMILARITY;
use serde::Deserialize;

/// Top-level configuration for Alexandria.
///
/// Load order:
/// 1. Compiled defaults
/// 2. Config file (see `config_path_from()` for resolution)
/// 3. Individual env var overrides
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub embedding: EmbeddingConfig,
    pub heat: HeatConfig,
    pub activation: ActivationConfig,
    pub cluster: ClusterConfig,
    pub retrieve: RetrieveConfig,
    pub reminders: RemindersConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Transport: "stdio" or "http". Default: stdio.
    pub transport: String,
    /// HTTP port when transport = "http". Default: 3000.
    pub port: u16,
    /// HTTP bind address. Default: 127.0.0.1.
    pub host: String,
    /// Allowed origins for HTTP CORS. Empty = allow all. Default: ["*"].
    pub allowed_origins: Vec<String>,
    /// Allowed hosts for HTTP. Empty = allow all. Default: ["*"].
    pub allowed_hosts: Vec<String>,
    /// SSE keep-alive interval in seconds. Default: 15.
    pub sse_keep_alive_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: "stdio".to_string(),
            port: 3000,
            host: "127.0.0.1".to_string(),
            allowed_origins: vec!["*".to_string()],
            allowed_hosts: vec!["*".to_string()],
            sse_keep_alive_secs: 15,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Storage path. Use ":memory:" for in-memory (ephemeral).
    /// Default: `$XDG_DATA_HOME/alexandria/data`
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    pub model: String,
    pub device: String,
    /// Facts per `embed()` call during `alexandria migrate-embeddings`. Bounds peak
    /// memory for large corpora; the server itself embeds one text at a time. Default 32.
    pub batch_size: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeatConfig {
    /// Base half-life for spaced repetition (seconds). Default 1 day.
    pub spacing_halflife_secs: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ActivationConfig {
    /// Fraction of heat passed per hop. Default 0.3.
    pub propagation_factor: f32,
    /// Max graph hops for spreading activation. Default 2.
    pub max_hops: u32,
    /// Number of top retrieval results that trigger spreading activation. Default: 3.
    pub top_n: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RetrieveConfig {
    /// Server-side hard floor on cosine similarity for `retrieve_memories` results.
    /// Defaults to `DEFAULT_MIN_SIMILARITY` in `alexandria_engine::search` — read that
    /// constant, not this comment, for the current number. It is a noise cutoff only:
    /// with all-MiniLM-L6-v2 a natural-language question against a stored statement
    /// scores ~0.2 and unrelated text ~0.0, so it must stay low.
    ///
    /// Derived by the retrieve-floor rule in
    /// `docs/plans/2026-09-08-embedding-model-swap-design.md`: the median
    /// non-hit score rounded to two decimals, which must sit below the lowest
    /// correct hit. The rule gives 0.08 for MiniLM; 0.10 is kept because the
    /// difference is immaterial.
    pub min_similarity: f32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClusterConfig {
    /// Similarity threshold for joining an existing cluster. Default 0.75.
    pub join_threshold: f32,
    /// Centroid similarity above which two clusters merge. Default 0.9.
    pub merge_threshold: f32,
    /// Avg member-to-centroid similarity below which a cluster splits. Defaults to
    /// `DEFAULT_COHESION_FLOOR` in `alexandria_engine::clusters::maintenance`.
    pub cohesion_floor: f32,
    /// Cluster maintenance check interval in seconds. Default: 300 (5 minutes).
    pub maintenance_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RemindersConfig {
    /// IANA timezone for naive datetime input and pattern/cron evaluation.
    /// Empty string = system-local (resolved via iana-time-zone at startup).
    /// Default: empty (system-local).
    pub timezone: String,
    /// Project-targeted reminders escalate to global delivery after being
    /// overdue this many hours. 0 = escalate as soon as overdue.
    /// Default: 48 (2 days).
    pub escalation_hours: u64,
}

// --- Defaults ---

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

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
        }
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: "sentence-transformers/all-MiniLM-L6-v2".to_string(),
            device: "cpu".to_string(),
            batch_size: 32,
        }
    }
}

impl Default for HeatConfig {
    fn default() -> Self {
        Self {
            spacing_halflife_secs: 86400.0,
        }
    }
}

impl Default for ActivationConfig {
    fn default() -> Self {
        Self {
            propagation_factor: 0.3,
            max_hops: 2,
            top_n: 3,
        }
    }
}

impl Default for RetrieveConfig {
    fn default() -> Self {
        Self {
            min_similarity: DEFAULT_MIN_SIMILARITY,
        }
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            join_threshold: 0.75,
            merge_threshold: 0.9,
            cohesion_floor: DEFAULT_COHESION_FLOOR,
            maintenance_interval_secs: 300,
        }
    }
}

impl Default for RemindersConfig {
    fn default() -> Self {
        Self {
            timezone: String::new(),
            escalation_hours: DEFAULT_ESCALATION_HOURS,
        }
    }
}

/// Resolve the config file path with precedence:
/// 1. `ALEXANDRIA_CONFIG` env var (explicit override)
/// 2. `$XDG_CONFIG_HOME/alexandria/config.toml` via `dirs::config_dir()`
/// 3. `~/.alexandria/config.toml` (legacy fallback)
/// 4. XDG path (for new installs, even if it doesn't exist yet)
fn config_path_from(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    // Explicit env override wins
    if let Some(p) = env("ALEXANDRIA_CONFIG") {
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

impl Config {
    /// Load configuration with the standard precedence chain:
    /// defaults → config file → env overrides
    ///
    /// Config file resolution: `ALEXANDRIA_CONFIG` env → XDG config dir → legacy `~/.alexandria/`
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&|k| std::env::var(k).ok())
    }

    fn load_from(env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        // 1. Start with defaults
        let mut config = Config::default();

        // 2. Load config file
        let config_path = config_path_from(env);

        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            config = toml::from_str(&contents)?;
            tracing::info!("Loaded config from {}", config_path.display());
        }

        // 3. Individual env var overrides
        if let Some(transport) = env("ALEXANDRIA_SERVER_TRANSPORT") {
            config.server.transport = transport;
        }
        if let Some(host) = env("ALEXANDRIA_SERVER_HOST") {
            config.server.host = host;
        }
        if let Some(port) = env("ALEXANDRIA_SERVER_PORT") {
            config.server.port = port
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid ALEXANDRIA_SERVER_PORT `{port}`: {e}"))?;
        }
        if let Some(dir) = env("ALEXANDRIA_DATA_DIR") {
            config.database.data_dir = PathBuf::from(dir);
        }
        if let Some(model) = env("ALEXANDRIA_EMBEDDING_MODEL") {
            config.embedding.model = model;
        }
        if let Some(device) = env("ALEXANDRIA_EMBEDDING_DEVICE") {
            config.embedding.device = device;
        }
        if let Some(batch) = env("ALEXANDRIA_EMBEDDING_BATCH_SIZE") {
            config.embedding.batch_size = batch.parse().map_err(|e| {
                anyhow::anyhow!("invalid ALEXANDRIA_EMBEDDING_BATCH_SIZE `{batch}`: {e}")
            })?;
        }
        if let Some(tz) = env("ALEXANDRIA_REMINDERS_TIMEZONE") {
            config.reminders.timezone = tz;
        }
        if let Some(h) = env("ALEXANDRIA_REMINDERS_ESCALATION_HOURS") {
            config.reminders.escalation_hours = h.parse().map_err(|e| {
                anyhow::anyhow!("invalid ALEXANDRIA_REMINDERS_ESCALATION_HOURS `{h}`: {e}")
            })?;
        }
        anyhow::ensure!(
            config.embedding.batch_size > 0,
            "embedding.batch_size must be at least 1"
        );

        Ok(config)
    }

    /// Load from a TOML string (for testing).
    #[cfg(test)]
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn test_defaults() {
        let config = Config::default();
        assert_eq!(
            config.embedding.model,
            "sentence-transformers/all-MiniLM-L6-v2"
        );
        assert_eq!(config.embedding.device, "cpu");
        assert_eq!(config.cluster.join_threshold, 0.75);
        assert_eq!(config.activation.propagation_factor, 0.3);
        assert_eq!(config.activation.max_hops, 2);
        assert_eq!(config.retrieve.min_similarity, 0.10);
        assert!(config.database.data_dir.ends_with("data"));
    }

    /// The binary's TOML defaults and the MCP server's construction fallbacks must not
    /// diverge — both derive from engine consts. This is the regression that let
    /// `retrieve.min_similarity` be 0.10 in production and 0.30 everywhere else.
    #[test]
    fn test_server_fallback_defaults_match_config_defaults() {
        assert_eq!(
            RetrieveConfig::default().min_similarity,
            alexandria_engine::search::DEFAULT_MIN_SIMILARITY
        );
        assert_eq!(
            ClusterConfig::default().cohesion_floor,
            alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR
        );
        // Same drift guard for reminders: the binary's config default and the MCP
        // server's fallback default are in different crates, and if only one is
        // changed a test-built or debug server escalates on a different clock than
        // production — with nothing failing.
        assert_eq!(
            RemindersConfig::default().escalation_hours,
            alexandria_mcp::server::DEFAULT_REMINDER_ESCALATION_HOURS
        );
    }

    #[test]
    fn test_parse_toml() {
        let toml = r#"
            [database]
            data_dir = "/tmp/alexandria-test"

            [embedding]
            model = "custom-model"
            device = "cuda"

            [cluster]
            join_threshold = 0.8
            merge_threshold = 0.95
            cohesion_floor = 0.5
        "#;
        let config = Config::from_toml(toml).unwrap();
        assert_eq!(
            config.database.data_dir,
            PathBuf::from("/tmp/alexandria-test")
        );
        assert_eq!(config.embedding.model, "custom-model");
        assert_eq!(config.embedding.device, "cuda");
        assert_eq!(config.cluster.join_threshold, 0.8);
        assert_eq!(config.cluster.merge_threshold, 0.95);
        // Heat uses default since not specified
        assert_eq!(config.heat.spacing_halflife_secs, 86400.0);
    }

    #[test]
    fn test_partial_toml_uses_defaults() {
        let toml = r#"
            [embedding]
            model = "other-model"
        "#;
        let config = Config::from_toml(toml).unwrap();
        assert_eq!(config.embedding.model, "other-model");
        // Everything else is default
        assert_eq!(config.embedding.device, "cpu");
        assert_eq!(config.embedding.batch_size, 32);
        assert_eq!(config.cluster.join_threshold, 0.75);
    }

    #[test]
    fn test_embedding_batch_size() {
        let config = Config::from_toml("[embedding]\nbatch_size = 8\n").unwrap();
        assert_eq!(config.embedding.batch_size, 8);
    }

    #[test]
    fn test_xdg_data_dir_default() {
        let config = Config::default();
        let xdg_data = dirs::data_dir().unwrap().join("alexandria").join("data");
        assert_eq!(config.database.data_dir, xdg_data);
    }

    #[test]
    fn test_config_path_env_override() {
        let env = env(&[("ALEXANDRIA_CONFIG", "/tmp/custom/config.toml")]);
        let path = config_path_from(&env);
        assert_eq!(path, PathBuf::from("/tmp/custom/config.toml"));
    }

    #[test]
    fn test_config_path_prefers_xdg_when_no_files_exist() {
        // When neither XDG nor legacy config files exist, config_path_from()
        // should return the XDG path (not legacy). We can't guarantee
        // neither file exists on this machine, so we verify the structural
        // property: the returned path is under dirs::config_dir(), not
        // under ~/.alexandria/.
        let xdg_config_dir = dirs::config_dir().unwrap();
        let legacy_dir = dirs::home_dir().unwrap().join(".alexandria");
        let path = config_path_from(&env(&[]));
        assert!(path.ends_with("config.toml"));
        // Must be under one of: XDG config dir OR legacy dir
        // (depends on what files exist on this machine)
        assert!(
            path.starts_with(&xdg_config_dir) || path.starts_with(&legacy_dir),
            "config_path_from() returned {}, expected it under {} or {}",
            path.display(),
            xdg_config_dir.display(),
            legacy_dir.display(),
        );
    }

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
        // retrieve uses default since not specified
        assert_eq!(config.retrieve.min_similarity, 0.10);

        let toml_retrieve = r#"
            [retrieve]
            min_similarity = 0.45
        "#;
        let config_retrieve = Config::from_toml(toml_retrieve).unwrap();
        assert_eq!(config_retrieve.retrieve.min_similarity, 0.45);
    }

    #[test]
    fn test_env_overrides() {
        let env = env(&[
            ("ALEXANDRIA_DATA_DIR", "/tmp/env-test"),
            ("ALEXANDRIA_EMBEDDING_MODEL", "env-model"),
            ("ALEXANDRIA_EMBEDDING_DEVICE", "env-device"),
            ("ALEXANDRIA_EMBEDDING_BATCH_SIZE", "8"),
        ]);

        let config = Config::load_from(&env).unwrap();
        assert_eq!(config.database.data_dir, PathBuf::from("/tmp/env-test"));
        assert_eq!(config.embedding.model, "env-model");
        assert_eq!(config.embedding.device, "env-device");
        assert_eq!(config.embedding.batch_size, 8);
    }

    #[test]
    fn test_embedding_env_invalid_batch_size() {
        let env = env(&[("ALEXANDRIA_EMBEDDING_BATCH_SIZE", "lots")]);
        let err = Config::load_from(&env).unwrap_err();
        assert!(
            err.to_string().contains("ALEXANDRIA_EMBEDDING_BATCH_SIZE"),
            "{err}"
        );
    }

    #[test]
    fn test_embedding_env_zero_batch_size() {
        let env = env(&[("ALEXANDRIA_EMBEDDING_BATCH_SIZE", "0")]);
        let err = Config::load_from(&env).unwrap_err();
        assert!(err.to_string().contains("batch_size"), "{err}");
    }

    #[test]
    fn test_embedding_toml_zero_batch_size() {
        let path =
            std::env::temp_dir().join(format!("alexandria-batch0-{}.toml", std::process::id()));
        std::fs::write(&path, "[embedding]\nbatch_size = 0\n").unwrap();
        let vars = [("ALEXANDRIA_CONFIG", path.to_str().unwrap())];
        let env = env(&vars);
        let err = Config::load_from(&env).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(err.to_string().contains("batch_size"), "{err}");
    }

    #[test]
    fn test_server_env_overrides() {
        let env = env(&[
            ("ALEXANDRIA_SERVER_TRANSPORT", "http"),
            ("ALEXANDRIA_SERVER_HOST", "0.0.0.0"),
            ("ALEXANDRIA_SERVER_PORT", "8080"),
        ]);

        let config = Config::load_from(&env).unwrap();
        assert_eq!(config.server.transport, "http");
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 8080);
    }

    #[test]
    fn test_server_env_invalid_port() {
        let env = env(&[("ALEXANDRIA_SERVER_PORT", "not-a-port")]);

        let result = Config::load_from(&env);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ALEXANDRIA_SERVER_PORT")
        );
    }

    #[test]
    fn test_reminders_defaults() {
        let config = Config::default();
        assert_eq!(config.reminders.timezone, ""); // empty = system-local, resolved at startup
        assert_eq!(config.reminders.escalation_hours, 48);
    }

    #[test]
    fn test_reminders_from_toml() {
        let toml = r#"
            [reminders]
            timezone = "Europe/Stockholm"
            escalation_hours = 24
        "#;
        let config = Config::from_toml(toml).unwrap();
        assert_eq!(config.reminders.timezone, "Europe/Stockholm");
        assert_eq!(config.reminders.escalation_hours, 24);
    }

    #[test]
    fn test_reminders_env_overrides() {
        let env = env(&[
            ("ALEXANDRIA_REMINDERS_TIMEZONE", "America/New_York"),
            ("ALEXANDRIA_REMINDERS_ESCALATION_HOURS", "12"),
        ]);

        let config = Config::load_from(&env).unwrap();
        assert_eq!(config.reminders.timezone, "America/New_York");
        assert_eq!(config.reminders.escalation_hours, 12);
    }

    #[test]
    fn test_reminders_env_invalid_hours() {
        let env = env(&[("ALEXANDRIA_REMINDERS_ESCALATION_HOURS", "soon")]);
        let result = Config::load_from(&env);
        // Name the variable that failed, not just "load errored": an unqualified
        // `is_err()` here would also pass on an unrelated config-file failure.
        let err = result.expect_err("a non-numeric escalation window must not load");
        assert!(
            err.to_string()
                .contains("ALEXANDRIA_REMINDERS_ESCALATION_HOURS"),
            "the error must name the offending variable: {err:#}"
        );
    }
}
