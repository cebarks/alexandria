use serde::{Deserialize, Serialize};

pub const GRANITE_MODEL_ID: &str = "ibm-granite/granite-embedding-311m-multilingual-r2";

/// Device-independent vector-space metadata. Legacy provenance remains unknown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EmbeddingSpace {
    Legacy {
        model: String,
        dimensions: usize,
        max_tokens: usize,
    },
    GraniteV1(Box<GraniteSpace>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraniteSpace {
    pub model: String,
    pub revision: String,
    pub onnx_sha256: String,
    pub burnpack_sha256: String,
    pub tokenizer_sha256: String,
    pub pooling: String,
    pub normalization: String,
    pub precision: String,
    pub semantics_version: u32,
    pub dimensions: usize,
    pub max_tokens: usize,
}

impl EmbeddingSpace {
    pub fn model(&self) -> &str {
        match self {
            Self::Legacy { model, .. } => model,
            Self::GraniteV1(space) => &space.model,
        }
    }

    pub fn dimensions(&self) -> usize {
        match self {
            Self::Legacy { dimensions, .. } => *dimensions,
            Self::GraniteV1(space) => space.dimensions,
        }
    }

    pub fn max_tokens(&self) -> usize {
        match self {
            Self::Legacy { max_tokens, .. } => *max_tokens,
            Self::GraniteV1(space) => space.max_tokens,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Legacy {
                model,
                dimensions,
                max_tokens,
            } => {
                anyhow::ensure!(
                    !model.trim().is_empty() && model != GRANITE_MODEL_ID,
                    "Invalid legacy model; Granite needs complete provenance"
                );
                anyhow::ensure!(
                    *dimensions > 0 && *max_tokens > 0,
                    "Invalid legacy dimensions/budget"
                );
                Ok(())
            }
            Self::GraniteV1(space) => space.validate(),
        }
    }
}

impl GraniteSpace {
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.model == GRANITE_MODEL_ID, "Unsupported Granite model");
        anyhow::ensure!(
            self.semantics_version == 1,
            "Unsupported embedding semantics version"
        );
        anyhow::ensure!(
            self.dimensions == 768,
            "Granite requires native768D vectors"
        );
        anyhow::ensure!(
            (2..=8192).contains(&self.max_tokens),
            "Granite token budget must be2..=8192"
        );
        for (field, actual, expected) in [
            ("pooling", self.pooling.as_str(), "cls"),
            (
                "normalization",
                self.normalization.as_str(),
                "l2-f64-accum-f32",
            ),
            ("precision", self.precision.as_str(), "f32"),
        ] {
            anyhow::ensure!(actual == expected, "Unsupported Granite {field}: {actual}");
        }
        anyhow::ensure!(
            lower_hex(&self.revision, 40),
            "Invalid pinned model revision"
        );
        for digest in [
            &self.onnx_sha256,
            &self.burnpack_sha256,
            &self.tokenizer_sha256,
        ] {
            anyhow::ensure!(lower_hex(digest, 64), "Invalid artifact SHA256 digest");
        }
        Ok(())
    }
}

/// Only final publication may enter `Complete`; checkpoints advance the work phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationPhase {
    Facts,
    Centroids,
    Verify,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReembedJournal {
    pub run_id: String,
    pub source_manifest_sha256: String,
    pub source_space: EmbeddingSpace,
    pub target_space: EmbeddingSpace,
    pub phase: MigrationPhase,
    pub last_fact_id: Option<surrealdb::types::RecordId>,
    pub reembedded_count: usize,
}

impl ReembedJournal {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.run_id.trim().is_empty(),
            "Migration run_id must not be blank"
        );
        anyhow::ensure!(
            lower_hex(&self.source_manifest_sha256, 64),
            "Invalid source manifest SHA256"
        );
        self.source_space.validate()?;
        self.target_space.validate()?;
        anyhow::ensure!(
            self.source_space.model() != "alexandria:embedding-migration-incomplete"
                && self.target_space.model() != "alexandria:embedding-migration-incomplete",
            "Migration sentinel is not an embedding space"
        );
        anyhow::ensure!(
            self.source_space.model() != self.target_space.model()
                || self.target_space.max_tokens() >= self.source_space.max_tokens(),
            "Migration cannot lower the token limit for the same model"
        );
        anyhow::ensure!(
            self.last_fact_id.is_some() == (self.reembedded_count > 0),
            "Migration cursor and reembedded_count disagree"
        );
        if let Some(cursor) = &self.last_fact_id {
            anyhow::ensure!(
                cursor.table.as_str() == "fact",
                "Migration cursor must identify a fact"
            );
        }
        Ok(())
    }
}

fn lower_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
