use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};

/// Stateless scope handle encoding a cluster context for progressive recall.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeHandle {
    pub cluster_id: String,
    pub depth: u32,
    pub issued_at: u64,
}

impl ScopeHandle {
    /// Encode the scope handle to a base64 string.
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        Ok(URL_SAFE_NO_PAD.encode(&json))
    }

    /// Decode a scope handle from a base64 string.
    pub fn decode(encoded: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
        let handle: Self = serde_json::from_slice(&bytes)?;
        Ok(handle)
    }
}
