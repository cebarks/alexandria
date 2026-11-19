use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    OpenAiChatCompletions,
    AnthropicMessages,
}

#[derive(Clone)]
pub enum HeaderValue {
    /// An explicitly non-secret inference setting (for example, an API version).
    Literal(String),
    SecretRef(String),
}

/// Operator input only. Do not serialize or debug-print credentials or header values.
#[derive(Clone)]
pub struct Route {
    pub protocol: Protocol,
    pub api_prefix: String,
    pub model: String,
    pub credential_ref: Option<String>,
    pub headers: BTreeMap<String, HeaderValue>,
    pub output_limit_field: Option<String>,
}

/// Canonical origin and API path, independent of credentials and inference settings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RouteDestinationId {
    pub protocol: Protocol,
    pub scheme: String,
    pub host: String,
    pub effective_port: u16,
    pub api_prefix: String,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid inference route")]
pub struct RouteError;

impl Route {
    pub fn destination(&self) -> Result<RouteDestinationId, RouteError> {
        let raw = &self.api_prefix;
        // The URL parser removes empty userinfo and treats backslashes as separators
        // for special schemes. Reject both before that normalization loses the evidence.
        let (scheme, after_scheme) = raw.split_once("://").ok_or(RouteError)?;
        let authority = after_scheme
            .split(['/', '?', '#'])
            .next()
            .ok_or(RouteError)?;
        if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            || authority.is_empty()
            || authority.contains('@')
            || raw.contains('\\')
            || raw
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
        {
            return Err(RouteError);
        }
        let parsed = Url::parse(raw).map_err(|_| RouteError)?;
        if !matches!(parsed.scheme(), "https" | "http")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(RouteError);
        }
        let path = parsed.path();
        // A single terminal slash is a base-URL separator. Do not collapse
        // repeated path slashes (or case) because a gateway can distinguish them.
        let prefix = if path != "/" && !path.ends_with("//") {
            path.strip_suffix('/').unwrap_or(path)
        } else {
            path
        };
        Ok(RouteDestinationId {
            protocol: self.protocol,
            scheme: parsed.scheme().to_owned(),
            host: parsed.host_str().ok_or(RouteError)?.to_owned(),
            effective_port: parsed
                .port_or_known_default()
                .filter(|port| *port != 0)
                .ok_or(RouteError)?,
            api_prefix: prefix.to_owned(),
        })
    }
}

pub struct RoutePair {
    proposer: Route,
    reviewer: Route,
    proposer_destination: RouteDestinationId,
    reviewer_destination: RouteDestinationId,
    reviewer_inherited: bool,
}

impl RoutePair {
    /// Only an omitted reviewer inherits. An explicit reviewer is a complete independent route.
    pub fn resolve(proposer: Route, reviewer: Option<Route>) -> Result<Self, RouteError> {
        if proposer.model.trim().is_empty()
            || reviewer
                .as_ref()
                .is_some_and(|route| route.model.trim().is_empty())
        {
            return Err(RouteError);
        }
        let proposer_destination = proposer.destination()?;
        let reviewer_inherited = reviewer.is_none();
        let (reviewer, reviewer_destination) = match reviewer {
            Some(route) => {
                let destination = route.destination()?;
                (route, destination)
            }
            None => (proposer.clone(), proposer_destination.clone()),
        };
        Ok(Self {
            proposer,
            reviewer,
            proposer_destination,
            reviewer_destination,
            reviewer_inherited,
        })
    }

    pub fn proposer(&self) -> &Route {
        &self.proposer
    }

    pub fn reviewer(&self) -> &Route {
        &self.reviewer
    }

    pub fn destination_set(&self) -> BTreeSet<RouteDestinationId> {
        BTreeSet::from([
            self.proposer_destination.clone(),
            self.reviewer_destination.clone(),
        ])
    }

    pub fn disclosure(&self) -> RouteDisclosure {
        RouteDisclosure {
            proposer: DisclosedRoute {
                destination: self.proposer_destination.clone(),
                model: self.proposer.model.clone(),
            },
            reviewer: DisclosedRoute {
                destination: self.reviewer_destination.clone(),
                model: self.reviewer.model.clone(),
            },
            reviewer_inherited: self.reviewer_inherited,
            same_model: self.proposer.model == self.reviewer.model,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDisclosure {
    pub proposer: DisclosedRoute,
    pub reviewer: DisclosedRoute,
    pub reviewer_inherited: bool,
    pub same_model: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisclosedRoute {
    pub destination: RouteDestinationId,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InferencePolicyId(pub String);

pub struct InferencePolicyInputs<'a> {
    pub prompt_template: &'a [u8],
    pub prompt_template_version: &'a str,
    pub operation_schema_version: &'a str,
    /// Allowlisted non-secret, canonicalized settings only; never an arbitrary config dump.
    pub settings: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct RouteIdentity<'a> {
    destination: &'a RouteDestinationId,
    model: &'a str,
    output_limit_field: Option<&'a str>,
    public_headers: BTreeMap<&'a str, &'a str>,
}

impl<'a> RouteIdentity<'a> {
    fn new(route: &'a Route, destination: &'a RouteDestinationId) -> Self {
        Self {
            destination,
            model: &route.model,
            output_limit_field: route.output_limit_field.as_deref(),
            public_headers: route
                .headers
                .iter()
                .filter_map(|(name, header)| match header {
                    HeaderValue::Literal(value) => Some((name.as_str(), value.as_str())),
                    HeaderValue::SecretRef(_) => None,
                })
                .collect(),
        }
    }
}

impl InferencePolicyId {
    pub fn fingerprint(routes: &RoutePair, inputs: &InferencePolicyInputs<'_>) -> Self {
        // Role order matters even if swapping proposer and reviewer leaves the
        // consent set unchanged. These structured fields contain no secret refs.
        let settings = serde_json::to_vec(&(
            RouteIdentity::new(&routes.proposer, &routes.proposer_destination),
            RouteIdentity::new(&routes.reviewer, &routes.reviewer_destination),
            inputs.settings,
        ))
        .expect(
            "inference identity fields contain only serializable scalars and string-keyed maps",
        );
        let mut digest = Sha256::new();
        digest.update(b"alexandria/refinement/inference-policy/v1\0");
        for field in [
            settings.as_slice(),
            inputs.prompt_template,
            inputs.prompt_template_version.as_bytes(),
            inputs.operation_schema_version.as_bytes(),
        ] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field);
        }
        Self(format!("{:x}", digest.finalize()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfiguredPolicy {
    pub destination_set: BTreeSet<RouteDestinationId>,
    pub inference_policy_id: InferencePolicyId,
    pub allow_automatic: bool,
}

/// The persisted configuration generation fences old work even after an A -> B -> A reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRevision {
    pub revision: u64,
    pub configuration: ConfiguredPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consent<A> {
    pub account: A,
    pub destination_set: BTreeSet<RouteDestinationId>,
    pub revision: u64,
    pub granted_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Human,
    SecondaryLlm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSetting {
    pub mode: ApprovalMode,
    pub revision: u64,
}

/// Capture at generation, and then keep that binding immutable for the concrete proposal revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority<A> {
    pub account: A,
    pub consent_revision: u64,
    pub owner_mode_revision: u64,
    pub inference_policy_id: InferencePolicyId,
    pub configured_revision: u64,
    pub mode: ApprovalMode,
}

/// Bind the admission snapshot to a new assessment. Persist this on the concrete
/// revision; opinion lookup and apply must never rebind an existing revision.
pub fn current_authority<A: Clone + Eq>(
    account: &A,
    policy: Option<&PolicyRevision>,
    consent: Option<&Consent<A>>,
    approval: ApprovalSetting,
) -> Option<Authority<A>> {
    let policy = policy?;
    let consent = consent?;
    if &consent.account != account
        || consent.revoked_at.is_some()
        || policy.configuration.destination_set.is_empty()
        || consent.destination_set != policy.configuration.destination_set
    {
        return None;
    }
    Some(Authority {
        account: account.clone(),
        consent_revision: consent.revision,
        owner_mode_revision: approval.revision,
        inference_policy_id: policy.configuration.inference_policy_id.clone(),
        configured_revision: policy.revision,
        mode: if policy.configuration.allow_automatic {
            approval.mode
        } else {
            ApprovalMode::Human
        },
    })
}

/// Currency for reuse in an inference assessment, not a veto over a human decision
/// on an existing concrete proposal whose source and evidence revisions still hold.
pub fn assessment_is_current<A: Eq>(origin: &Authority<A>, current: Option<&Authority<A>>) -> bool {
    current.is_some_and(|binding| {
        origin.account == binding.account
            && origin.consent_revision == binding.consent_revision
            && origin.configured_revision == binding.configured_revision
            && origin.inference_policy_id == binding.inference_policy_id
    })
}

pub fn automatic_verdict_is_current<A: Eq>(
    origin: &Authority<A>,
    current: Option<&Authority<A>>,
    approved_exact_revision: bool,
) -> bool {
    approved_exact_revision && origin.mode == ApprovalMode::SecondaryLlm && current == Some(origin)
}

#[cfg(test)]
mod tests;
