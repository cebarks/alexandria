use super::*;

fn route(model: &str, endpoint: &str) -> Route {
    Route {
        protocol: Protocol::OpenAiChatCompletions,
        api_prefix: endpoint.into(),
        model: model.into(),
        credential_ref: Some("proposer-key-reference".into()),
        headers: BTreeMap::from([(
            "x-gateway-key".into(),
            HeaderValue::SecretRef("proposer-gateway-key-reference".into()),
        )]),
        output_limit_field: Some("max_tokens".into()),
    }
}

const ENDPOINT: &str = "https://inference.example/gateway/v1";

fn canonical_destination() -> RouteDestinationId {
    RouteDestinationId {
        protocol: Protocol::OpenAiChatCompletions,
        scheme: "https".into(),
        host: "inference.example".into(),
        effective_port: 443,
        api_prefix: "/gateway/v1".into(),
    }
}

// Most policy tests use already-resolved routes so a parser defect cannot mask a policy defect.
fn pair(proposer_model: &str, reviewer_model: &str) -> RoutePair {
    RoutePair {
        proposer: route(proposer_model, ENDPOINT),
        reviewer: route(reviewer_model, ENDPOINT),
        proposer_destination: canonical_destination(),
        reviewer_destination: canonical_destination(),
        reviewer_inherited: false,
    }
}

#[test]
fn destination_identity_canonicalizes_origin_without_losing_gateway_prefix() {
    let mut proposer = route("proposer", "HTTPS://INFERENCE.example:443/gateway/v1/");
    assert_eq!(proposer.destination().ok(), Some(canonical_destination()));
    assert_eq!(
        route("other", ENDPOINT).destination().unwrap(),
        canonical_destination()
    );

    for (endpoint, host, port, prefix) in [
        (
            "https://elsewhere.example/gateway/v1",
            "elsewhere.example",
            443,
            "/gateway/v1",
        ),
        (
            "https://inference.example:444/gateway/v1",
            "inference.example",
            444,
            "/gateway/v1",
        ),
        (
            "https://inference.example/Gateway/v1",
            "inference.example",
            443,
            "/Gateway/v1",
        ),
    ] {
        proposer.api_prefix = endpoint.into();
        let actual = proposer.destination().unwrap();
        assert_ne!(actual, canonical_destination());
        assert_eq!(
            (
                actual.host.as_str(),
                actual.effective_port,
                actual.api_prefix.as_str()
            ),
            (host, port, prefix)
        );
    }
    let plain = route("proposer", "http://inference.example/gateway/v1")
        .destination()
        .unwrap();
    assert_eq!((plain.scheme.as_str(), plain.effective_port), ("http", 80));
    let mut anthropic = route("proposer", ENDPOINT);
    anthropic.protocol = Protocol::AnthropicMessages;
    assert_ne!(anthropic.destination().unwrap(), canonical_destination());
}

#[test]
fn destination_rejects_url_userinfo_query_and_fragment() {
    assert!(
        route("m", ENDPOINT).destination().is_ok(),
        "a clean URL must be accepted"
    );
    for endpoint in [
        "https://user@inference.example/gateway/v1",
        "https://user:password@inference.example/gateway/v1",
        "https://@inference.example/gateway/v1",
        "https://inference.example/gateway/v1?",
        "https://inference.example/gateway/v1?arg=1",
        "https://inference.example/gateway/v1#",
        "https://inference.example/gateway/v1#note",
        "file:///gateway/v1",
    ] {
        assert!(
            route("m", endpoint).destination().is_err(),
            "accepted {endpoint}"
        );
    }
}

#[test]
fn same_model_route_inheritance_is_accepted_and_disclosed() {
    let resolved = RoutePair::resolve(route("shared-model", ENDPOINT), None);
    assert!(
        resolved.is_ok(),
        "omitted reviewer must inherit the proposer route"
    );
    let resolved = resolved.unwrap();
    let disclosure = resolved.disclosure();
    assert!(disclosure.reviewer_inherited);
    assert!(disclosure.same_model);
    assert_eq!(disclosure.proposer.model, "shared-model");
    assert_eq!(disclosure.proposer, disclosure.reviewer);
    assert_eq!(
        resolved.destination_set(),
        BTreeSet::from([canonical_destination()])
    );
    assert_eq!(
        resolved.reviewer().credential_ref.as_deref(),
        Some("proposer-key-reference")
    );
    let public = serde_json::to_string(&disclosure).unwrap();
    assert!(!public.contains("proposer-key-reference"));
    assert!(!public.contains("proposer-gateway-key-reference"));
    assert!(!public.contains("x-gateway-key"));
}

#[test]
fn explicit_reviewer_does_not_inherit_credentials_across_hosts() {
    let mut reviewer = route("shared-model", "https://review.example/alternate/v1");
    reviewer.credential_ref = None;
    reviewer.headers.clear();
    let resolved = RoutePair::resolve(route("shared-model", ENDPOINT), Some(reviewer));
    assert!(
        resolved.is_ok(),
        "a complete credentialless override must be accepted"
    );
    let resolved = resolved.unwrap();
    assert!(resolved.reviewer().credential_ref.is_none());
    assert!(resolved.reviewer().headers.is_empty());
    assert!(!resolved.disclosure().reviewer_inherited);
    assert!(resolved.disclosure().same_model);
    assert_eq!(resolved.destination_set().len(), 2);
    let disclosure = resolved.disclosure();
    let reviewer = &disclosure.reviewer.destination;
    assert_eq!(reviewer.host, "review.example");
    assert_eq!(reviewer.api_prefix, "/alternate/v1");
}

fn identity(
    routes: &RoutePair,
    prompt: &[u8],
    prompt_version: &str,
    schema_version: &str,
) -> InferencePolicyId {
    InferencePolicyId::fingerprint(
        routes,
        &InferencePolicyInputs {
            prompt_template: prompt,
            prompt_template_version: prompt_version,
            operation_schema_version: schema_version,
            settings: &BTreeMap::from([("reasoning_effort".into(), "low".into())]),
        },
    )
}

fn standard_identity(routes: &RoutePair) -> InferencePolicyId {
    identity(routes, b"template", "prompt-v1", "schema-v1")
}

#[test]
fn key_rotation_alone_changes_neither_destination_nor_inference_policy() {
    let mut proposer = route("m", ENDPOINT);
    let before = RoutePair::resolve(proposer.clone(), None);
    assert!(
        before.is_ok(),
        "the original route must resolve before rotating keys"
    );
    proposer.credential_ref = Some("rotated-key-ref".into());
    proposer.headers.insert(
        "x-gateway-key".into(),
        HeaderValue::SecretRef("rotated-header-ref".into()),
    );
    let after = RoutePair::resolve(proposer, None).unwrap();
    let before = before.unwrap();
    assert_eq!(before.destination_set(), after.destination_set());
    assert_eq!(standard_identity(&before), standard_identity(&after));
    assert_eq!(before.disclosure(), after.disclosure());
}

#[test]
fn non_secret_inference_settings_invalidate_the_policy_fingerprint() {
    let routes = pair("a", "b");
    let changed = InferencePolicyId::fingerprint(
        &routes,
        &InferencePolicyInputs {
            prompt_template: b"template",
            prompt_template_version: "prompt-v1",
            operation_schema_version: "schema-v1",
            settings: &BTreeMap::from([("reasoning_effort".into(), "high".into())]),
        },
    );
    assert_ne!(
        standard_identity(&routes),
        changed,
        "settings must be fingerprinted"
    );
    let mut changed_routes = pair("a", "b");
    changed_routes.proposer.output_limit_field = Some("max_completion_tokens".into());
    assert_ne!(
        standard_identity(&routes),
        standard_identity(&changed_routes)
    );
    let mut changed_routes = pair("a", "b");
    changed_routes
        .reviewer
        .headers
        .insert("x-api-version".into(), HeaderValue::Literal("v2".into()));
    assert_ne!(
        standard_identity(&routes),
        standard_identity(&changed_routes)
    );
}

const ACCOUNT: &str = "owner-a";
const NOW: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;
const LLM: ApprovalSetting = ApprovalSetting {
    mode: ApprovalMode::SecondaryLlm,
    revision: 2,
};
const HUMAN: ApprovalSetting = ApprovalSetting {
    mode: ApprovalMode::Human,
    revision: 1,
};

fn policy(revision: u64, inference_policy_id: InferencePolicyId) -> PolicyRevision {
    PolicyRevision {
        revision,
        configuration: ConfiguredPolicy {
            destination_set: BTreeSet::from([canonical_destination()]),
            inference_policy_id,
            allow_automatic: true,
        },
    }
}

fn grant() -> Consent<&'static str> {
    Consent {
        account: ACCOUNT,
        destination_set: BTreeSet::from([canonical_destination()]),
        revision: 7,
        granted_at: NOW,
        revoked_at: None,
    }
}

fn authority(
    policy: &PolicyRevision,
    consent: &Consent<&'static str>,
    mode: ApprovalSetting,
) -> Authority<&'static str> {
    let authority = current_authority(&ACCOUNT, Some(policy), Some(consent), mode);
    assert_eq!(
        authority.as_ref().map(|a| a.mode),
        Some(mode.mode),
        "granted current policy must bind authority in the owner mode"
    );
    authority.unwrap()
}

#[test]
fn missing_configuration_or_consent_cannot_bind_authority() {
    let policy = policy(1, standard_identity(&pair("a", "b")));
    let consent = grant();
    let binding = authority(&policy, &consent, LLM);
    assert!(automatic_verdict_is_current(&binding, Some(&binding), true));
    assert!(current_authority(&ACCOUNT, None, Some(&consent), LLM).is_none());
    assert!(current_authority(&ACCOUNT, Some(&policy), None, LLM).is_none());
    assert!(current_authority(&"owner-b", Some(&policy), Some(&consent), LLM).is_none());
}

#[test]
fn secondary_destination_override_requires_renewal() {
    let routes = pair("a", "b");
    let old = policy(1, standard_identity(&routes));
    let consent = grant();
    let origin = authority(&old, &consent, LLM);
    let mut new = policy(2, standard_identity(&routes));
    new.configuration
        .destination_set
        .insert(RouteDestinationId {
            host: "review.example".into(),
            ..canonical_destination()
        });
    let current = current_authority(&ACCOUNT, Some(&new), Some(&consent), LLM);
    assert!(current.is_none());
    assert!(!automatic_verdict_is_current(
        &origin,
        current.as_ref(),
        true
    ));
    let renewed = Consent {
        destination_set: new.configuration.destination_set.clone(),
        revision: 8,
        ..consent
    };
    let current = authority(&new, &renewed, LLM);
    assert!(!automatic_verdict_is_current(&origin, Some(&current), true));
}

#[test]
fn disabling_consent_or_automation_invalidates_late_authority() {
    let mut policy = policy(1, standard_identity(&pair("a", "b")));
    let mut consent = grant();
    let origin = authority(&policy, &consent, LLM);
    assert!(automatic_verdict_is_current(&origin, Some(&origin), true));
    assert!(!automatic_verdict_is_current(&origin, Some(&origin), false));

    let off = authority(
        &policy,
        &consent,
        ApprovalSetting {
            mode: ApprovalMode::Human,
            revision: 3,
        },
    );
    assert!(!automatic_verdict_is_current(&origin, Some(&off), true));
    let back_on = authority(&policy, &consent, ApprovalSetting { revision: 4, ..LLM });
    assert!(!automatic_verdict_is_current(&origin, Some(&back_on), true));
    policy.configuration.allow_automatic = false;
    policy.revision = 2;
    let by_ceiling = current_authority(&ACCOUNT, Some(&policy), Some(&consent), LLM);
    assert_eq!(
        by_ceiling.as_ref().map(|a| a.mode),
        Some(ApprovalMode::Human)
    );
    assert!(!automatic_verdict_is_current(
        &origin,
        by_ceiling.as_ref(),
        true
    ));
    policy.configuration.allow_automatic = true;
    policy.revision = 3;
    let ceiling_restored = authority(&policy, &consent, LLM);
    assert!(!automatic_verdict_is_current(
        &origin,
        Some(&ceiling_restored),
        true
    ));
    consent.revoked_at = Some(NOW);
    consent.revision = 8;
    let revoked = current_authority(&ACCOUNT, Some(&policy), Some(&consent), LLM);
    assert!(revoked.is_none());
    assert!(!automatic_verdict_is_current(
        &origin,
        revoked.as_ref(),
        true
    ));
    consent.revoked_at = None;
    consent.revision = 9;
    let renewed = authority(&policy, &consent, LLM);
    assert!(!automatic_verdict_is_current(
        &ceiling_restored,
        Some(&renewed),
        true
    ));
}

#[test]
fn switching_to_llm_mode_never_auto_applies_human_mode_backlog() {
    let policy = policy(1, standard_identity(&pair("a", "b")));
    let consent = grant();
    let human_origin = authority(&policy, &consent, HUMAN);
    let current = authority(&policy, &consent, LLM);

    // Bind authority at generation, not when the old opinion is fetched or when apply is attempted.
    let prior_opinion_approves = true;
    assert!(!automatic_verdict_is_current(
        &human_origin,
        Some(&current),
        prior_opinion_approves
    ));
    let explicitly_regenerated_origin = authority(&policy, &consent, LLM);
    assert!(automatic_verdict_is_current(
        &explicitly_regenerated_origin,
        Some(&current),
        true
    ));
}

fn assert_inference_change(initial: InferencePolicyId, changed: InferencePolicyId) {
    assert_ne!(
        initial, changed,
        "changed inference inputs must have a new policy ID"
    );
    let old = policy(1, initial);
    let new = policy(2, changed);
    let consent = grant();
    let origin = authority(&old, &consent, LLM);
    let changed_id_only = policy(old.revision, new.configuration.inference_policy_id.clone());
    let different_id = authority(&changed_id_only, &consent, LLM);
    assert!(!assessment_is_current(&origin, Some(&different_id)));
    assert!(!automatic_verdict_is_current(
        &origin,
        Some(&different_id),
        true
    ));
    assert!(assessment_is_current(&origin, Some(&origin)));
    assert!(automatic_verdict_is_current(&origin, Some(&origin), true));
    let current = authority(&new, &consent, LLM);
    assert_eq!(current.consent_revision, origin.consent_revision);
    assert!(!assessment_is_current(&origin, Some(&current)));
    assert!(!automatic_verdict_is_current(&origin, Some(&current), true));
    assert!(automatic_verdict_is_current(&current, Some(&current), true));

    let restored = policy(3, old.configuration.inference_policy_id.clone());
    let restored = authority(&restored, &consent, LLM);
    assert!(!assessment_is_current(&origin, Some(&restored)));
    assert!(!automatic_verdict_is_current(
        &origin,
        Some(&restored),
        true
    ));
}

#[test]
fn model_change_invalidates_automatic_authority_without_renewing_destination_consent() {
    let routes = pair("old-proposer", "old-reviewer");
    for change in [
        pair("new-proposer", "old-reviewer"),
        pair("old-proposer", "new-reviewer"),
    ] {
        assert_inference_change(standard_identity(&routes), standard_identity(&change));
    }
}

#[test]
fn prompt_change_invalidates_automatic_authority_without_renewing_destination_consent() {
    let routes = pair("a", "b");
    for changed in [
        identity(&routes, b"different template", "prompt-v1", "schema-v1"),
        identity(&routes, b"template", "prompt-v2", "schema-v1"),
    ] {
        assert_inference_change(standard_identity(&routes), changed);
    }
}

#[test]
fn schema_change_invalidates_automatic_authority_without_renewing_destination_consent() {
    let routes = pair("a", "b");
    assert_inference_change(
        standard_identity(&routes),
        identity(&routes, b"template", "prompt-v1", "schema-v2"),
    );
}

#[test]
fn disclosure_shows_model_when_destination_unchanged() {
    let before = pair("old-proposer", "reviewer").disclosure();
    let after = pair("new-proposer", "reviewer").disclosure();
    assert_eq!(after.proposer.model, "new-proposer");
    assert_eq!(before.proposer.model, "old-proposer");
    assert_eq!(after.proposer.destination, before.proposer.destination);
    assert_eq!(after.reviewer.model, "reviewer");
}
