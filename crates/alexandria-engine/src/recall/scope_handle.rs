use alexandria_storage::models::ClusterVersion;
use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

/// The handle progressive recall issues for one cluster and accepts on the next call.
///
/// A **locator and a version, nothing else**. It is not a capability: the string is unsigned and clients
/// keep and replay it, so the server resolves `cluster_id` under the *caller's own* context on every
/// single use before it reads a member — which is what makes a handle that was logged, guessed or handed
/// over worth nothing to whoever holds it. It carries no account, no credential and no nonce-shaped
/// secret, because any of those in a client-held string would be a thing a later reader could mistake
/// for authority, and none of them is needed to name a cluster the caller can already read.
///
/// `version` is what stops the locator implying a frozen snapshot. The handle this type replaces carried
/// `depth` (always zero) and `issued_at` (a wall clock) and read nothing from the store, so replaying
/// one after a membership change looked like continuity and was not.
///
/// The wire field is still `scope_handle`: that name is in the advertised tool schema, and renaming it
/// would be a public change for a word.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallHandle {
    pub cluster_id: String,
    /// `#[serde(default)]` because a handle minted before this field existed carries no version, and an
    /// in-flight client must not start failing mid-conversation: the absent stamp is the version the
    /// store starts a cluster at, and the server serves current data either way.
    #[serde(default)]
    pub version: ClusterVersion,
}

impl RecallHandle {
    /// The handle for one cluster, stamped with the version the issuing read saw.
    pub fn new(cluster_id: impl Into<String>, version: ClusterVersion) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            version,
        }
    }

    /// Encode the handle to the opaque string a client echoes back.
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self)?;
        Ok(URL_SAFE_NO_PAD.encode(&json))
    }

    /// Whether the cluster has moved since this handle was issued.
    ///
    /// Disclosure, not rejection: a focused read answers with the caller's current cluster contents
    /// whatever this says, and what the comparison makes impossible is the opposite claim — that the
    /// rows handed back are the snapshot the handle described.
    pub fn snapshot_moved(&self, current: &ClusterVersion) -> bool {
        &self.version != current
    }

    /// Decode a client-supplied handle.
    ///
    /// One refusal text for every unreadable input — not base64, not JSON, not the missing field's name.
    /// Those failures are not equivalent *internally*, but the caller chose the string, and an answer
    /// that reports which layer gave up tells it something this server has no reason to say: the
    /// decoder's shape is not part of the contract, and quoting it puts engine text into a tool
    /// response. `every_undecodable_handle_gets_the_same_refusal_and_no_decoder_text` is the witness.
    ///
    /// A handle minted before `version` existed decodes rather than being refused: it carries no stamp,
    /// which is a claim of nothing, and refusing it would break a deployed conversation for no
    /// additional safety. `a_handle_issued_before_the_version_field_still_decodes` pins that.
    pub fn decode(encoded: &str) -> Result<Self> {
        let refused = || anyhow!("malformed recall handle");
        let bytes = match URL_SAFE_NO_PAD.decode(encoded) {
            Ok(bytes) => bytes,
            Err(_) => return Err(refused()),
        };
        serde_json::from_slice(&bytes).map_err(|_| refused())
    }
}
