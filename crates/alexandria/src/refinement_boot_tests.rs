use alexandria_storage::access::AccessRepo;
use alexandria_storage::models::identity::OperationId;
use alexandria_storage::models::refinement::ConsentChoice;
use alexandria_storage::repos::{AuthRepo, IdentityRepo, RefinementRepo};
use chrono::Utc;

use super::*;

const CONFIGURED: &str = r#"
[refinement]
approval_mode_ceiling = "secondary_llm"

[refinement.proposer]
protocol = "open_ai_chat_completions"
api_prefix = "https://boot-fixture.example/gateway/v1"
model = "boot-fixture"
credential_ref = "REFINEMENT_BOOT_TEST_KEY"
"#;

#[tokio::test]
async fn refuses_startup_when_policy_reconciliation_is_refused() {
    let mut db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let mut unrelated_db = Database::connect_embedded().await.unwrap();
    let unrelated_system = unrelated_db.claim_system_authority().unwrap();
    let config = Config::from_toml(CONFIGURED).unwrap();

    let boot = reconcile_refinement_at_boot(&db, &unrelated_system, &config.refinement).await;
    assert!(
        boot.is_err(),
        "boot must propagate the repository's reconciliation refusal"
    );
    let error = boot.err().unwrap();
    assert!(format!("{error:#}").contains("reconcile refinement policy"));

    let system = db.claim_system_authority().unwrap();
    assert!(
        reconcile_refinement_at_boot(&db, &system, &config.refinement)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn boot_publishes_configuration_without_consent_and_removal_disables_it() {
    let mut db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let system = db.claim_system_authority().unwrap();
    let digest = AuthRepo::create_password_digest("refinement-boot-fixture-password-padding")
        .await
        .unwrap();
    let account = IdentityRepo::new(db.inner())
        .bootstrap_owner(
            "refinement-boot-owner",
            digest.as_str(),
            &OperationId::new("refinement-boot-owner"),
        )
        .await
        .unwrap()
        .account;
    let access = AccessRepo::new(&db);
    let principal = access.local_principal(&system, &account).await.unwrap();
    let owner = access.authorize(&principal).await.unwrap();
    let repo = RefinementRepo::new(&db);
    let config = Config::from_toml(CONFIGURED).unwrap();

    reconcile_refinement_at_boot(&db, &system, &config.refinement)
        .await
        .unwrap();
    let unconsented = repo.effective_policy(&owner).await.unwrap();
    assert!(
        unconsented.configured.is_some(),
        "boot must persist the operator policy before serving"
    );
    assert!(!unconsented.inference_admitted);
    assert!(unconsented.consent.is_none());
    let published = unconsented.configured.unwrap();
    assert!(published.configuration.allow_automatic);
    repo.set_consent(
        &owner,
        0,
        ConsentChoice::Grant {
            disclosed_policy_revision: published.revision,
        },
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(
        repo.effective_policy(&owner)
            .await
            .unwrap()
            .inference_admitted
    );

    reconcile_refinement_at_boot(&db, &system, &Config::default().refinement)
        .await
        .unwrap();
    let disabled = repo.effective_policy(&owner).await.unwrap();
    assert!(disabled.configured.is_none());
    assert!(!disabled.inference_admitted);
    assert!(
        disabled.consent.is_some(),
        "configuration removal disables admission without erasing history"
    );
}
