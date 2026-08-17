use std::path::PathBuf;

use serde::Deserialize;

/// Top-level configuration for Alexandria.
///
/// Load order:
/// 1. Compiled defaults
/// 2. `~/.alexandria/config.toml` (if exists)
/// 3. `ALEXANDRIA_CONFIG` env path (if set)
/// 4. Individual env var overrides
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            transport: "stdio".to_string(),
            port: 3000,
            host: "127.0.0.1".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Storage path. Use ":memory:" for in-memory (ephemeral).
    /// Default: ~/.alexandria/data
    pub data_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    pub model: String,
    pub device: String,
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClusterConfig {
    /// Similarity threshold for joining an existing cluster. Default 0.75.
    pub join_threshold: f32,
    /// Centroid similarity above which two clusters merge. Default 0.9.
    pub merge_threshold: f32,
    /// Avg member-to-centroid similarity below which a cluster splits. Default 0.6.
    pub cohesion_floor: f32,
}

// --- Defaults ---

fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".alexandria")
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
        }
    }
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            join_threshold: 0.75,
            merge_threshold: 0.9,
            cohesion_floor: 0.6,
        }
    }
}

impl Config {
    /// Load configuration with the standard precedence chain:
    /// defaults → ~/.alexandria/config.toml → ALEXANDRIA_CONFIG → env overrides
    pub fn load() -> anyhow::Result<Self> {
        // 1. Start with defaults
        let mut config = Config::default();

        // 2. Try ~/.alexandria/config.toml
        let default_path = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".alexandria")
            .join("config.toml");

        // 3. ALEXANDRIA_CONFIG overrides the default path
        let config_path = std::env::var("ALEXANDRIA_CONFIG")
            .map(PathBuf::from)
            .unwrap_or(default_path);

        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            config = toml::from_str(&contents)?;
            tracing::info!("Loaded config from {}", config_path.display());
        }

        // 4. Individual env var overrides
        if let Ok(dir) = std::env::var("ALEXANDRIA_DATA_DIR") {
            config.database.data_dir = PathBuf::from(dir);
        }
        if let Ok(model) = std::env::var("ALEXANDRIA_EMBEDDING_MODEL") {
            config.embedding.model = model;
        }
        if let Ok(device) = std::env::var("ALEXANDRIA_EMBEDDING_DEVICE") {
            config.embedding.device = device;
        }

        Ok(config)
    }

    /// Load from a TOML string (for testing).
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(config.database.data_dir.ends_with("data"));
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
        assert_eq!(config.cluster.join_threshold, 0.75);
    }

    #[test]
    fn test_env_overrides() {
        // Set env vars
        std::env::set_var("ALEXANDRIA_DATA_DIR", "/tmp/env-test");
        std::env::set_var("ALEXANDRIA_EMBEDDING_MODEL", "env-model");

        let config = Config::load().unwrap();
        assert_eq!(config.database.data_dir, PathBuf::from("/tmp/env-test"));
        assert_eq!(config.embedding.model, "env-model");

        // Clean up
        std::env::remove_var("ALEXANDRIA_DATA_DIR");
        std::env::remove_var("ALEXANDRIA_EMBEDDING_MODEL");
    }
}
