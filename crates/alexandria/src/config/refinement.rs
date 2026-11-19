use std::{collections::BTreeMap, fmt};

use alexandria_engine::refinement::policy::{HeaderValue, Protocol, Route, RoutePair};
use alexandria_storage::models::refinement::ApprovalMode;
use anyhow::Context;
use axum::http::HeaderName;
use serde::{Deserialize, Deserializer};

#[cfg(test)]
mod tests;

/// Operator configuration only. No credentials are read while loading this section.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefinementConfig {
    pub approval_mode_ceiling: ApprovalMode,
    pub proposer: Option<RouteConfig>,
    pub reviewer: Option<RouteConfig>,
}

impl Default for RefinementConfig {
    fn default() -> Self {
        Self {
            approval_mode_ceiling: ApprovalMode::Human,
            proposer: None,
            reviewer: None,
        }
    }
}

// Decode the operator subtree without returning deserializer details that can quote input values.
pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<RefinementConfig, D::Error>
where
    D: Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    value.try_into().map_err(|_| {
        serde::de::Error::custom(
            "invalid [refinement] configuration; routes require protocol, api_prefix and model, \
             with optional credential_ref, header_refs and output_limit_field",
        )
    })
}

impl RefinementConfig {
    pub fn resolve(&self) -> anyhow::Result<Option<RoutePair>> {
        let Some(proposer) = &self.proposer else {
            anyhow::ensure!(
                self.reviewer.is_none(),
                "refinement.reviewer requires refinement.proposer"
            );
            return Ok(None);
        };
        let proposer = proposer.resolve("refinement.proposer")?;
        let reviewer = self
            .reviewer
            .as_ref()
            .map(|route| route.resolve("refinement.reviewer"))
            .transpose()?;
        Ok(Some(RoutePair::resolve(proposer, reviewer)?))
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub protocol: Protocol,
    pub api_prefix: String,
    pub model: String,
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub header_refs: BTreeMap<String, String>,
    pub output_limit_field: Option<String>,
}

impl fmt::Debug for RouteConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteConfig")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl RouteConfig {
    fn resolve(&self, section: &str) -> anyhow::Result<Route> {
        anyhow::ensure!(
            !self.model.trim().is_empty(),
            "{section}.model must not be blank"
        );
        anyhow::ensure!(
            self.credential_ref
                .as_deref()
                .is_none_or(is_environment_name),
            "{section}.credential_ref must be an environment variable name, not a credential value"
        );
        let mut headers = BTreeMap::new();
        for (name, reference) in &self.header_refs {
            anyhow::ensure!(
                is_environment_name(reference),
                "{section}.header_refs values must be environment variable names"
            );
            let name = HeaderName::from_bytes(name.as_bytes()).with_context(|| {
                format!("{section}.header_refs contains an invalid header name")
            })?;
            anyhow::ensure!(
                headers
                    .insert(
                        name.as_str().to_owned(),
                        HeaderValue::SecretRef(reference.clone())
                    )
                    .is_none(),
                "{section}.header_refs contains duplicate header names"
            );
        }
        let route = Route {
            protocol: self.protocol,
            api_prefix: self.api_prefix.clone(),
            model: self.model.clone(),
            credential_ref: self.credential_ref.clone(),
            headers,
            output_limit_field: self.output_limit_field.clone(),
        };
        route.destination().with_context(|| {
            format!(
                "{section}.api_prefix must be an HTTP(S) URL without userinfo, query or fragment"
            )
        })?;
        Ok(route)
    }
}

fn is_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
