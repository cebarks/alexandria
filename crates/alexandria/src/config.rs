use std::path::PathBuf;

use alexandria_engine::clusters::maintenance::DEFAULT_COHESION_FLOOR;
use alexandria_engine::dreaming::{
    DEFAULT_APPRAISE_INTERVAL_SECS, DEFAULT_CLUSTER_INTERVAL_SECS, DEFAULT_COLD_HEAT_FLOOR,
    DEFAULT_COLLAPSE_INTERVAL_SECS, DEFAULT_DEMOTE_CONFIDENCE_CEILING, DEFAULT_MAX_ROWS_PER_RUN,
    DEFAULT_MERGE_INTERVAL_SECS, DEFAULT_SWEEP_INTERVAL_SECS, DEMOTED_CONFIDENCE, Intervals,
};
use alexandria_engine::heat::DEFAULT_DECAY_TAU_SECS;
use alexandria_engine::heat::DEFAULT_SPACING_REFERENCE_SECS;
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
    pub dreaming: DreamingConfig,
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
    /// Facts per `embed()` call during `alexandria migrate-embeddings`, which is also
    /// how often it writes and logs progress. It does not bound memory: the Candle
    /// provider runs one forward pass per text whatever the call size. 1..=4096,
    /// default 32; the server itself embeds one text at a time.
    pub batch_size: usize,
    /// Longest text, in wordpiece tokens including `[CLS]`/`[SEP]`, one embedding sees; the
    /// rest of a longer text is not searchable. 3..=512, default 256 (what
    /// sentence-transformers serves this model at, and the only value besides 128 that has
    /// been measured); above that is experimental. A corpus stamped before this key existed
    /// is treated as 128 and must be migrated before it will boot at the default. Locked on
    /// first boot: raising it needs `alexandria migrate-embeddings`, and it is never lowered.
    pub max_tokens: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HeatConfig {
    /// Base decay time constant (seconds). `tau = stability * this`; lower cools faster.
    /// Default 1 day, matching the value `projected_heat` used before it was configurable.
    pub decay_tau_secs: f64,
    /// Access gap (seconds) at which one access earns full stability growth; a shorter gap earns
    /// a proportional fraction. Lower means stability accrues from less widely spaced accesses.
    pub spacing_reference_secs: f64,
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
    /// A constant kept by hand. `alexandria bench-retrieval` prints a
    /// retrieve-floor rule (median non-hit score rounded to two decimals, valid
    /// only below the weakest correct hit), but its output depends on the corpus
    /// and does not move in one direction — 0.08 at 143 facts, 0.07 at 807,
    /// 0.08 at 957. 0.10 sits above all of them and far below the weakest true
    /// hit (0.338). See `docs/minilm-test-data.md`.
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
            max_tokens: alexandria_pipeline::embedding::DEFAULT_MAX_TOKENS,
        }
    }
}

impl Default for HeatConfig {
    fn default() -> Self {
        Self {
            decay_tau_secs: DEFAULT_DECAY_TAU_SECS,
            spacing_reference_secs: DEFAULT_SPACING_REFERENCE_SECS,
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
        }
    }
}

/// Background housekeeping: one scheduler, five independently-due jobs. HTTP mode only, same as the
/// cluster maintenance loop it replaces — stdio has no long-lived process to run a clock in.
///
/// Every interval defaults to a constant in `alexandria_engine::dreaming` rather than to a literal
/// here, because the same numbers bound the scheduler's due-time tests. Two homes for one default
/// is how `min_similarity` ended up drifting between config and server.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DreamingConfig {
    /// Master switch. `false` means the loop is never spawned, so no job writes anything.
    pub enabled: bool,
    /// Heat materialisation. Default 3600 (1 hour).
    pub sweep_interval_secs: u64,
    /// Cohesion check → split. Default 300 (5 minutes), the old cluster-maintenance cadence.
    pub cluster_interval_secs: u64,
    /// Centroid similarity → merge. Default 300. Split from `cluster` because merge is the
    /// expensive half and an operator who wants it rarer should not pay for slower splits.
    pub merge_interval_secs: u64,
    /// Byte-identical duplicate collapse. Default 86400 (1 day).
    pub collapse_interval_secs: u64,
    /// Cold-row demotion. Default 86400 (1 day).
    pub appraise_interval_secs: u64,
    /// Rows one job may **write** per run. Default 500.
    ///
    /// Two of the five jobs read more than this and must, so the bound is on the writes and not on
    /// the reads: `collapse` needs every live fact's content before it can group duplicates at all,
    /// and `appraise` reads every live fact's confidence and store time even though it pages only
    /// this many heat rows. `JobReport::examined` reports what was read, so on an N-fact corpus
    /// collapse says `examined: N` while acting on at most this many rows — which is the honest
    /// shape, and the reason the key does not say what a corpus drains per tick.
    pub max_rows_per_run: usize,
    /// Projected heat at or below which a memory counts as cold. Default
    /// [`DEFAULT_COLD_HEAT_FLOOR`]. PROVISIONAL — see that constant.
    pub cold_heat_floor: f64,
    /// Stored confidence at or below which a cold, never-accessed memory is demotable. Default
    /// [`DEFAULT_DEMOTE_CONFIDENCE_CEILING`]. PROVISIONAL — see that constant.
    pub demote_confidence_ceiling: f64,
}

impl Default for DreamingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sweep_interval_secs: DEFAULT_SWEEP_INTERVAL_SECS,
            cluster_interval_secs: DEFAULT_CLUSTER_INTERVAL_SECS,
            merge_interval_secs: DEFAULT_MERGE_INTERVAL_SECS,
            collapse_interval_secs: DEFAULT_COLLAPSE_INTERVAL_SECS,
            appraise_interval_secs: DEFAULT_APPRAISE_INTERVAL_SECS,
            max_rows_per_run: DEFAULT_MAX_ROWS_PER_RUN,
            cold_heat_floor: DEFAULT_COLD_HEAT_FLOOR,
            demote_confidence_ceiling: DEFAULT_DEMOTE_CONFIDENCE_CEILING,
        }
    }
}

impl DreamingConfig {
    /// The cadences in the shape the engine's scheduler wants.
    pub fn intervals(&self) -> Intervals {
        Intervals {
            sweep_secs: self.sweep_interval_secs,
            cluster_secs: self.cluster_interval_secs,
            merge_secs: self.merge_interval_secs,
            collapse_secs: self.collapse_interval_secs,
            appraise_secs: self.appraise_interval_secs,
        }
    }

    /// Every non-zero interval, paired with the key name an operator would edit. A zero interval
    /// means "due on every tick" in the engine, which turns a daily pass into a hot loop, so the
    /// binary refuses to start on one — the same posture as the escalation-window check in
    /// `main.rs`, which refuses a window too large to represent.
    pub fn interval_keys(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("dreaming.sweep_interval_secs", self.sweep_interval_secs),
            ("dreaming.cluster_interval_secs", self.cluster_interval_secs),
            ("dreaming.merge_interval_secs", self.merge_interval_secs),
            (
                "dreaming.collapse_interval_secs",
                self.collapse_interval_secs,
            ),
            (
                "dreaming.appraise_interval_secs",
                self.appraise_interval_secs,
            ),
        ]
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

/// Warnings for keys a config file still sets that this release removed.
///
/// Kept as a pure function over the parsed TOML rather than an inline `tracing::warn!` in the
/// loader: `main.rs`'s closure and the file-reading path are both unreachable from a test without
/// module mocking, and this warning is the only migration signal an operator gets.
pub(crate) fn removed_key_warnings(raw: &toml::Value) -> Vec<&'static str> {
    let mut warnings = Vec::new();

    if raw
        .get("heat")
        .and_then(|heat| heat.get("spacing_halflife_secs"))
        .is_some()
    {
        warnings.push(
            "[heat] spacing_halflife_secs was removed: it named a half-life but was used as the \
             spacing denominator, and its documented direction was the reverse of the behaviour. \
             Set [heat] decay_tau_secs (base decay time constant; lower cools faster) and/or \
             [heat] spacing_reference_secs (access gap earning full stability growth; lower means \
             shorter gaps earn full credit) instead.",
        );
    }

    if raw
        .get("cluster")
        .and_then(|cluster| cluster.get("maintenance_interval_secs"))
        .is_some()
    {
        warnings.push(
            "[cluster] maintenance_interval_secs was removed: cluster maintenance is no longer one \
             loop with one cadence, it is two of the five dreaming jobs. Set \
             dreaming.cluster_interval_secs (cohesion check, splits) and/or \
             dreaming.merge_interval_secs (centroid similarity, merges) instead, and \
             dreaming.enabled = false if you want the background work off.",
        );
    }

    warnings
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
            // Parse generically first: deserialising straight into `Config` lets
            // `#[serde(default)]` drop a removed key in silence, which is the wrong failure mode
            // for a breaking change — the operator's tuned value stops applying and nothing says
            // so. The warning names the replacements, so grepping the service log for the old key
            // finds the migration path.
            let raw: toml::Value = toml::from_str(&contents)?;
            for warning in removed_key_warnings(&raw) {
                tracing::warn!("{warning}");
            }
            config = raw.try_into()?;
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
        if let Some(tokens) = env("ALEXANDRIA_EMBEDDING_MAX_TOKENS") {
            config.embedding.max_tokens = tokens.parse().map_err(|e| {
                anyhow::anyhow!("invalid ALEXANDRIA_EMBEDDING_MAX_TOKENS `{tokens}`: {e}")
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
            (1..=4096).contains(&config.embedding.batch_size),
            "embedding.batch_size must be between 1 and 4096"
        );
        // 3 is `[CLS]`, one wordpiece, `[SEP]`. 512 is the BERT position table; the provider
        // checks the loaded model's own table too.
        anyhow::ensure!(
            (3..=512).contains(&config.embedding.max_tokens),
            "embedding.max_tokens must be between 3 and 512"
        );
        // The scheduler's own arithmetic treats a zero interval as "due on every tick", so a typo
        // here would turn a daily pass into a hot loop that never sleeps. Refusing to start says
        // which key is wrong instead of burning a core.
        for (key, interval) in config.dreaming.interval_keys() {
            anyhow::ensure!(
                interval > 0,
                "{key} must be greater than 0 (a zero interval makes the job due on every tick). \
                 Set dreaming.enabled = false to turn the scheduler off instead."
            );
        }
        anyhow::ensure!(
            config.dreaming.max_rows_per_run > 0,
            "dreaming.max_rows_per_run must be greater than 0, or every job examines nothing and \
             reports success"
        );
        anyhow::ensure!(
            config.dreaming.demote_confidence_ceiling > DEMOTED_CONFIDENCE,
            "dreaming.demote_confidence_ceiling must be above {DEMOTED_CONFIDENCE}, the value a \
             demoted memory is written at, or appraise can never demote anything"
        );
        // The tuning values the intervals' zero-guard does not cover, and they need it more: each one
        // silently inverts a job rather than stopping it. `projected_heat` uses
        // `tau = stability * decay_tau_secs`, so a zero tau projects *every* row to heat 0, which
        // makes `is_cold` true corpus-wide — that is the mass-demotion vector the arming gate exists
        // to close, reopened by a typo in a different key. A floor of 0 disables appraise with no
        // signal and a floor of 1.0 or more makes everything cold; a zero spacing reference makes the
        // denominator in `on_access` either infinite or a division by zero. Refusing by name is the
        // same posture as the escalation-window check in `main.rs`.
        anyhow::ensure!(
            config.heat.decay_tau_secs.is_finite() && config.heat.decay_tau_secs > 0.0,
            "heat.decay_tau_secs must be a positive number of seconds; zero projects every memory to \
             heat 0, which makes appraise treat the whole corpus as cold"
        );
        anyhow::ensure!(
            config.heat.spacing_reference_secs.is_finite()
                && config.heat.spacing_reference_secs > 0.0,
            "heat.spacing_reference_secs must be a positive number of seconds; it is the denominator \
             `on_access` grows stability with, and 0 stops stability growing at all"
        );
        anyhow::ensure!(
            config.dreaming.cold_heat_floor.is_finite()
                && config.dreaming.cold_heat_floor > 0.0
                && config.dreaming.cold_heat_floor < 1.0,
            "dreaming.cold_heat_floor must be strictly between 0 and 1 (a fraction of the 1.0 a fresh \
             access writes); 0 disables appraise and 1.0 or more makes every memory cold"
        );
        // An interval so large the job never runs is the mirror image of a zero interval, and just as
        // likely to be a unit mistake (milliseconds written into a seconds field, or the reverse).
        const MAX_INTERVAL_SECS: u64 = 365 * 24 * 60 * 60;
        for (key, interval) in config.dreaming.interval_keys() {
            anyhow::ensure!(
                interval <= MAX_INTERVAL_SECS,
                "{key} must be at most {MAX_INTERVAL_SECS} (one year); a larger value means the job \
                 never runs, which is the same outcome as turning the scheduler off"
            );
        }

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
        // And for heat: the binary's `[heat]` defaults and the MCP server's construction
        // fallback are in different crates, so a change to one alone makes a test-built or
        // debug server cool memories on a different clock than production — with nothing failing.
        let heat = HeatConfig::default();
        let server_heat = alexandria_mcp::server::HeatSettings::default();
        assert_eq!(heat.decay_tau_secs, server_heat.decay_tau_secs);
        assert_eq!(
            heat.spacing_reference_secs,
            server_heat.spacing_reference_secs
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
        assert_eq!(config.heat.decay_tau_secs, 86400.0);
        assert_eq!(config.heat.spacing_reference_secs, 86400.0);
    }

    /// The engine constants are the single home for these defaults. A config default that
    /// drifted would make a test-built server decay differently from production — the
    /// `min_similarity` 0.10-vs-0.30 lesson, re-armed for `[heat]`.
    #[test]
    fn heat_config_defaults_match_engine_defaults() {
        let defaults = HeatConfig::default();
        assert_eq!(defaults.decay_tau_secs, DEFAULT_DECAY_TAU_SECS);
        assert_eq!(
            defaults.spacing_reference_secs,
            DEFAULT_SPACING_REFERENCE_SECS
        );
    }

    /// `#[serde(default)]` would otherwise swallow the removed key and the operator's tuned value
    /// would stop applying with no signal at all.
    #[test]
    fn a_config_using_the_removed_heat_key_is_warned_about() {
        let raw: toml::Value =
            toml::from_str("[heat]\nspacing_halflife_secs = 3600.0\n").expect("valid toml");
        let warnings = removed_key_warnings(&raw);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        for needle in [
            "spacing_halflife_secs",
            "decay_tau_secs",
            "spacing_reference_secs",
        ] {
            assert!(
                warnings[0].contains(needle),
                "warning must name {needle} so an operator grepping the log for either the old \
                 key or a replacement finds the migration path"
            );
        }
    }

    #[test]
    fn current_heat_keys_produce_no_warning() {
        let raw: toml::Value =
            toml::from_str("[heat]\ndecay_tau_secs = 3600.0\nspacing_reference_secs = 7200.0\n")
                .expect("valid toml");
        assert!(removed_key_warnings(&raw).is_empty());
    }

    /// `cluster.maintenance_interval_secs` moved rather than vanished: one loop with one cadence
    /// became two of the five dreaming jobs. `#[serde(default)]` would swallow the old key, so the
    /// warning has to name both replacements — an operator who tuned 600 needs to know which of the
    /// two it fed.
    #[test]
    fn a_config_using_the_removed_cluster_interval_key_is_warned_about() {
        let raw: toml::Value =
            toml::from_str("[cluster]\nmaintenance_interval_secs = 600\n").expect("valid toml");
        let warnings = removed_key_warnings(&raw);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        for needle in [
            "maintenance_interval_secs",
            "dreaming.cluster_interval_secs",
            "dreaming.merge_interval_secs",
            "dreaming.enabled",
        ] {
            assert!(
                warnings[0].contains(needle),
                "warning must name {needle} so an operator grepping the log for either the old \
                 key or a replacement finds the migration path"
            );
        }
    }

    #[test]
    fn current_dreaming_keys_produce_no_warning() {
        let raw: toml::Value = toml::from_str(
            "[dreaming]\nenabled = true\ncluster_interval_secs = 600\nmerge_interval_secs = 900\n",
        )
        .expect("valid toml");
        assert!(removed_key_warnings(&raw).is_empty(), "{:?}", raw);
    }

    /// Same drift guard as the heat and reminders ones above: the binary's defaults must be the
    /// engine's constants, because those constants are what the scheduler's due-time tests pin.
    #[test]
    fn test_dreaming_defaults_derive_from_engine_constants() {
        let defaults = DreamingConfig::default();
        assert!(defaults.enabled, "the scheduler is on by default");
        assert_eq!(defaults.sweep_interval_secs, DEFAULT_SWEEP_INTERVAL_SECS);
        assert_eq!(
            defaults.cluster_interval_secs,
            DEFAULT_CLUSTER_INTERVAL_SECS
        );
        assert_eq!(defaults.merge_interval_secs, DEFAULT_MERGE_INTERVAL_SECS);
        assert_eq!(
            defaults.collapse_interval_secs,
            DEFAULT_COLLAPSE_INTERVAL_SECS
        );
        assert_eq!(
            defaults.appraise_interval_secs,
            DEFAULT_APPRAISE_INTERVAL_SECS
        );
        assert_eq!(defaults.max_rows_per_run, DEFAULT_MAX_ROWS_PER_RUN);
        assert_eq!(defaults.cold_heat_floor, DEFAULT_COLD_HEAT_FLOOR);
        assert_eq!(
            defaults.demote_confidence_ceiling,
            DEFAULT_DEMOTE_CONFIDENCE_CEILING
        );

        let intervals = defaults.intervals();
        assert_eq!(intervals, Intervals::default());
        assert_eq!(
            intervals.interval_for(alexandria_engine::dreaming::Job::Collapse),
            DEFAULT_COLLAPSE_INTERVAL_SECS
        );
    }

    #[test]
    fn test_dreaming_toml_overrides_intervals() {
        let path =
            std::env::temp_dir().join(format!("alexandria-dream-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "[dreaming]\nenabled = false\nsweep_interval_secs = 60\nmax_rows_per_run = 25\n",
        )
        .unwrap();
        let config =
            Config::load_from(&env(&[("ALEXANDRIA_CONFIG", path.to_str().unwrap())])).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!config.dreaming.enabled);
        assert_eq!(config.dreaming.sweep_interval_secs, 60);
        assert_eq!(config.dreaming.max_rows_per_run, 25);
        assert_eq!(
            config.dreaming.collapse_interval_secs, DEFAULT_COLLAPSE_INTERVAL_SECS,
            "keys the operator left alone keep their defaults"
        );
    }

    /// A zero interval means "due on every tick" in the engine, so a typo turns a daily pass into a
    /// hot loop. Refusing to start names the key instead of burning a core.
    #[test]
    fn test_dreaming_toml_zero_interval_refuses_to_start() {
        for (key, needle) in [
            ("sweep_interval_secs", "dreaming.sweep_interval_secs"),
            ("cluster_interval_secs", "dreaming.cluster_interval_secs"),
            ("merge_interval_secs", "dreaming.merge_interval_secs"),
            ("collapse_interval_secs", "dreaming.collapse_interval_secs"),
            ("appraise_interval_secs", "dreaming.appraise_interval_secs"),
        ] {
            let path = std::env::temp_dir().join(format!(
                "alexandria-dream0-{}-{key}.toml",
                std::process::id()
            ));
            std::fs::write(&path, format!("[dreaming]\n{key} = 0\n")).unwrap();
            let err = Config::load_from(&env(&[("ALEXANDRIA_CONFIG", path.to_str().unwrap())]))
                .unwrap_err();
            std::fs::remove_file(&path).unwrap();
            assert!(
                err.to_string().contains(needle),
                "{key} must be refused by name: {err}"
            );
            assert!(
                err.to_string().contains("dreaming.enabled"),
                "the refusal must name the supported way to turn the scheduler off: {err}"
            );
        }
    }

    /// The mirror image of the zero-interval refusal: every tuning key the scheduler reads must be
    /// refused when it would silently invert a job instead of stopping it. Each case here is a value
    /// that loads cleanly, boots, and then makes `Appraise` treat the whole corpus as cold (or do
    /// nothing forever) with no line of output to say so.
    #[test]
    fn test_dreaming_toml_inverted_tuning_values_refuse_to_start() {
        for (toml, needle) in [
            ("[heat]\ndecay_tau_secs = 0.0\n", "heat.decay_tau_secs"),
            ("[heat]\ndecay_tau_secs = -1.0\n", "heat.decay_tau_secs"),
            (
                "[heat]\nspacing_reference_secs = 0.0\n",
                "heat.spacing_reference_secs",
            ),
            (
                "[dreaming]\ncold_heat_floor = 0.0\n",
                "dreaming.cold_heat_floor",
            ),
            (
                "[dreaming]\ncold_heat_floor = 1.0\n",
                "dreaming.cold_heat_floor",
            ),
            (
                "[dreaming]\nappraise_interval_secs = 40000000000\n",
                "dreaming.appraise_interval_secs",
            ),
        ] {
            let path = std::env::temp_dir().join(format!(
                "alexandria-tuning-{}-{}.toml",
                std::process::id(),
                needle.replace(['.', '_'], "-")
            ));
            std::fs::write(&path, toml).unwrap();
            let err = Config::load_from(&env(&[("ALEXANDRIA_CONFIG", path.to_str().unwrap())]))
                .unwrap_err();
            std::fs::remove_file(&path).unwrap();
            assert!(
                err.to_string().contains(needle),
                "{toml} must be refused by naming the key the operator would edit; got: {err}"
            );
        }
    }

    #[test]
    fn test_dreaming_toml_zero_max_rows_refuses_to_start() {
        let path =
            std::env::temp_dir().join(format!("alexandria-rows0-{}.toml", std::process::id()));
        std::fs::write(&path, "[dreaming]\nmax_rows_per_run = 0\n").unwrap();
        let err =
            Config::load_from(&env(&[("ALEXANDRIA_CONFIG", path.to_str().unwrap())])).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(err.to_string().contains("max_rows_per_run"), "{err}");
    }

    /// A ceiling at or below the value demoted memories are written at makes the predicate
    /// unsatisfiable, so `appraise` would run forever and demote nothing.
    #[test]
    fn test_dreaming_demote_ceiling_below_the_demoted_value_refuses_to_start() {
        let path =
            std::env::temp_dir().join(format!("alexandria-ceil-{}.toml", std::process::id()));
        std::fs::write(&path, "[dreaming]\ndemote_confidence_ceiling = 0.1\n").unwrap();
        let err =
            Config::load_from(&env(&[("ALEXANDRIA_CONFIG", path.to_str().unwrap())])).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(
            err.to_string().contains("demote_confidence_ceiling"),
            "{err}"
        );
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
        assert_eq!(
            config.dreaming.cluster_interval_secs, DEFAULT_CLUSTER_INTERVAL_SECS,
            "the cadence the old cluster.maintenance_interval_secs controlled"
        );
        assert_eq!(config.activation.top_n, 3);
    }

    #[test]
    fn test_new_config_from_toml() {
        let toml = r#"
            [server]
            sse_keep_alive_secs = 30

            [dreaming]
            cluster_interval_secs = 600

            [activation]
            top_n = 5
        "#;
        let config = Config::from_toml(toml).unwrap();
        assert_eq!(config.server.sse_keep_alive_secs, 30);
        assert_eq!(config.dreaming.cluster_interval_secs, 600);
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
    fn test_embedding_max_tokens_default_env_and_range() {
        assert_eq!(Config::default().embedding.max_tokens, 256);
        // 128 is still reachable: it is what a pre-lock corpus is stamped at, so a database
        // that has not been migrated has to be able to express it.
        let config = Config::from_toml("[embedding]\nmax_tokens = 128\n").unwrap();
        assert_eq!(config.embedding.max_tokens, 128);

        let at =
            |v: &'static str| Config::load_from(&env(&[("ALEXANDRIA_EMBEDDING_MAX_TOKENS", v)]));
        assert_eq!(at("512").unwrap().embedding.max_tokens, 512);
        for bad in ["2", "513"] {
            let err = at(bad).unwrap_err().to_string();
            assert!(err.contains("between 3 and 512"), "{bad}: {err}");
        }
        let err = at("many").unwrap_err().to_string();
        assert!(err.contains("ALEXANDRIA_EMBEDDING_MAX_TOKENS"), "{err}");
    }

    #[test]
    fn test_embedding_env_zero_batch_size() {
        let env = env(&[("ALEXANDRIA_EMBEDDING_BATCH_SIZE", "0")]);
        let err = Config::load_from(&env).unwrap_err();
        assert!(err.to_string().contains("batch_size"), "{err}");
    }

    #[test]
    fn test_embedding_env_batch_size_upper_bound() {
        let ok = env(&[("ALEXANDRIA_EMBEDDING_BATCH_SIZE", "4096")]);
        assert_eq!(Config::load_from(&ok).unwrap().embedding.batch_size, 4096);
        let over = env(&[("ALEXANDRIA_EMBEDDING_BATCH_SIZE", "4097")]);
        let err = Config::load_from(&over).unwrap_err();
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
