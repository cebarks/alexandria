use alexandria_engine::refinement::policy::HeaderValue;

use super::super::Config;
use super::*;

const PROPOSER: &str = r#"
[refinement.proposer]
protocol = "open_ai_chat_completions"
api_prefix = "https://inference.example/gateway/v1/"
model = "primary"
credential_ref = "REFINEMENT_PRIMARY_KEY"
output_limit_field = "max_completion_tokens"

[refinement.proposer.header_refs]
x-gateway-key = "REFINEMENT_GATEWAY_KEY"
"#;

fn load(contents: &str) -> anyhow::Result<Config> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, contents).unwrap();
    Config::load_from(&|key| {
        (key == "ALEXANDRIA_CONFIG").then(|| path.to_str().unwrap().to_owned())
    })
}

#[test]
fn proposer_only_resolves_with_referenced_credentials_and_human_ceiling() {
    let config = load(PROPOSER).unwrap().refinement;
    assert_eq!(config.approval_mode_ceiling, ApprovalMode::Human);
    let configured = config.resolve().unwrap();
    assert!(
        configured.is_some(),
        "a complete proposer config must resolve routes"
    );
    let configured = configured.unwrap();
    let routes = configured.disclosure();
    assert!(routes.reviewer_inherited && routes.same_model);
    assert_eq!(routes.proposer.destination.api_prefix, "/gateway/v1");
    assert_eq!(
        configured.proposer().credential_ref.as_deref(),
        Some("REFINEMENT_PRIMARY_KEY")
    );
    assert_eq!(
        configured.reviewer().credential_ref.as_deref(),
        Some("REFINEMENT_PRIMARY_KEY")
    );
    assert!(matches!(configured.proposer().headers.get("x-gateway-key"),
        Some(HeaderValue::SecretRef(name)) if name == "REFINEMENT_GATEWAY_KEY"));
}

#[test]
fn reviewer_without_proposer_is_rejected_at_config_load() {
    let config = load(&PROPOSER.replace("refinement.proposer", "refinement.reviewer"));
    assert!(
        config.is_err(),
        "a reviewer cannot enable refinement without a proposer"
    );
    assert!(format!("{:#}", config.unwrap_err()).contains("refinement.proposer"));
}

#[test]
fn route_url_userinfo_is_rejected_without_echoing_it() {
    let invalid = PROPOSER.replace(
        "https://inference.example",
        "https://fixture-user:fixture-password@inference.example",
    );
    let config = load(&invalid);
    assert!(
        config.is_err(),
        "route userinfo must not be accepted as inline credentials"
    );
    let error = format!("{:#}", config.unwrap_err());
    assert!(error.contains("refinement.proposer"));
    assert!(!error.contains("fixture-password"));
}

#[test]
fn value_instead_of_credential_reference_is_rejected_without_echoing_it() {
    let invalid = PROPOSER.replace("REFINEMENT_PRIMARY_KEY", "Bearer fixture-not-a-real-secret");
    let config = load(&invalid);
    assert!(
        config.is_err(),
        "credential_ref must be a reference, not a bearer value"
    );
    let error = format!("{:#}", config.unwrap_err());
    assert!(error.contains("credential_ref"));
    assert!(!error.contains("fixture-not-a-real-secret"));
}
