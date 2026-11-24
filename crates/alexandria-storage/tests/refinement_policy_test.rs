use std::collections::BTreeSet;

use alexandria_storage::access::{AccessRepo, OwnerContext, Principal, SystemContext};
use alexandria_storage::error::StorageError;
use alexandria_storage::models::identity::{AccountId, OperationId};
use alexandria_storage::models::refinement::{
    ApprovalMode, ConfiguredPolicy, Consent, ConsentChoice, EffectivePolicy, InferencePolicyId,
    PolicyRevision, Protocol, RouteDestinationId,
};
use alexandria_storage::repos::{
    AdminRepo, AuthRepo, IdentityRepo, PasswordDigest, RefinementRepo,
};
use alexandria_storage::{Database, schema};
use chrono::{DateTime, TimeDelta, Utc};
use tokio::sync::OnceCell;

const NOW: DateTime<Utc> = DateTime::<Utc>::UNIX_EPOCH;

struct Fixture {
    db: Database,
    system: SystemContext,
    admin_principal: Principal,
    admin: OwnerContext,
    user: OwnerContext,
}

impl Fixture {
    async fn new() -> Self {
        static DIGEST: OnceCell<PasswordDigest> = OnceCell::const_new();
        let digest = DIGEST
            .get_or_init(|| async {
                AuthRepo::create_password_digest("refinement-repo-fixture-password-padding")
                    .await
                    .unwrap()
            })
            .await;
        let mut db = Database::connect_embedded().await.unwrap();
        let system = db.claim_system_authority().unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let identity = IdentityRepo::new(db.inner());
        let admin = identity
            .bootstrap_owner(
                "refinement-admin",
                digest.as_str(),
                &OperationId::new("refinement-admin"),
            )
            .await
            .unwrap();
        let user = identity
            .bootstrap_owner(
                "refinement-user",
                digest.as_str(),
                &OperationId::new("refinement-user"),
            )
            .await
            .unwrap();
        db.inner()
            .query("UPDATE ONLY $target SET role = 'admin' RETURN NONE")
            .bind(("target", admin.account.as_record_id().clone()))
            .await
            .unwrap()
            .check()
            .unwrap();
        let access = AccessRepo::new(&db);
        let admin_principal = access
            .local_principal(&system, &admin.account)
            .await
            .unwrap();
        let user_principal = access
            .local_principal(&system, &user.account)
            .await
            .unwrap();
        let admin = access.authorize(&admin_principal).await.unwrap();
        let user = access.authorize(&user_principal).await.unwrap();
        Self {
            db,
            system,
            admin_principal,
            admin,
            user,
        }
    }

    fn repo(&self) -> RefinementRepo<'_> {
        RefinementRepo::new(&self.db)
    }

    async fn configure(&self, configured: &ConfiguredPolicy) -> PolicyRevision {
        let result = self
            .repo()
            .reconcile_policy(&self.system, Some(configured))
            .await;
        assert!(
            matches!(&result, Ok(Some(_))),
            "operator configuration must publish a policy revision: {result:?}"
        );
        result.unwrap().unwrap()
    }

    async fn grant(&self, owner: &OwnerContext, disclosed_policy_revision: u64) -> Consent {
        let result = self
            .repo()
            .set_consent(
                owner,
                0,
                ConsentChoice::Grant {
                    disclosed_policy_revision,
                },
                NOW,
            )
            .await;
        assert!(
            matches!(&result, Ok(consent) if consent.revision == 1),
            "first explicit grant must create consent revision 1: {result:?}"
        );
        result.unwrap()
    }
}

fn destination(host: &str) -> RouteDestinationId {
    RouteDestinationId {
        protocol: Protocol::OpenAiChatCompletions,
        scheme: "https".into(),
        host: host.into(),
        effective_port: 443,
        api_prefix: "/gateway/v1".into(),
    }
}

fn policy() -> ConfiguredPolicy {
    ConfiguredPolicy {
        destination_set: BTreeSet::from([destination("inference.example")]),
        inference_policy_id: InferencePolicyId("a".repeat(64)),
        allow_automatic: true,
    }
}

async fn effective(repo: &RefinementRepo<'_>, owner: &OwnerContext) -> EffectivePolicy {
    let result = repo.effective_policy(owner).await;
    assert!(
        result.is_ok(),
        "active owner must receive an effective policy: {result:?}"
    );
    result.unwrap()
}

#[tokio::test]
async fn v011_defines_refinement_tables_alongside_the_existing_semantic_journal() {
    let db = Database::connect_embedded().await.unwrap();
    schema::migrate(db.inner()).await.unwrap();
    let v011 = schema::MIGRATIONS
        .iter()
        .find(|(version, _, _)| *version == 11)
        .expect("v011 is a registered migration");
    assert_eq!(v011.1, "refinement");
    assert!(schema::LATEST_VERSION >= v011.0);
    let info: Vec<serde_json::Value> = db
        .inner()
        .query("INFO FOR DB")
        .await
        .unwrap()
        .take(0)
        .unwrap();
    let info = &info[0];
    for table in [
        "refinement_proposal",
        "refinement_attempt",
        "refinement_consent",
        "refinement_budget",
        "refinement_evidence",
        "refinement_policy",
        "refinement_approval_mode",
        "semantic_operation",
        "semantic_snapshot",
    ] {
        assert!(
            info["tables"][table]
                .as_str()
                .is_some_and(|definition| definition.contains("SCHEMAFULL")),
            "missing schemafull table {table}"
        );
    }
}

#[tokio::test]
async fn no_consent_by_default() {
    let fixture = Fixture::new().await;
    let repo = fixture.repo();
    assert!(
        matches!(repo.consent(&fixture.user).await, Ok(None)),
        "fresh owner has no consent row"
    );
    let mode = repo.approval_mode(&fixture.user).await.unwrap();
    assert_eq!(mode.mode, ApprovalMode::Human);
    assert_eq!(mode.revision, 0);
    let resolved = effective(&repo, &fixture.user).await;
    assert!(resolved.configured.is_none());
    assert!(!resolved.inference_admitted);
}

#[tokio::test]
async fn server_url_setting_does_not_opt_users_in() {
    let fixture = Fixture::new().await;
    let configured = fixture.configure(&policy()).await;
    let repo = fixture.repo();
    let resolved = effective(&repo, &fixture.user).await;
    assert_eq!(resolved.configured, Some(configured.clone()));
    assert!(!resolved.inference_admitted);
    assert!(
        !repo
            .scheduled_policy(&fixture.system, fixture.user.account())
            .await
            .unwrap()
            .inference_admitted
    );
    let consent = fixture.grant(&fixture.user, configured.revision).await;
    assert_eq!(consent.account, *fixture.user.account());
    assert_eq!(consent.granted_at, NOW);
    assert!(effective(&repo, &fixture.user).await.inference_admitted);
    assert!(
        repo.scheduled_policy(&fixture.system, fixture.user.account())
            .await
            .unwrap()
            .inference_admitted
    );
    assert!(!effective(&repo, &fixture.admin).await.inference_admitted);
}

#[tokio::test]
async fn secondary_destination_override_requires_disclosed_renewal() {
    let fixture = Fixture::new().await;
    let mut config = policy();
    let first = fixture.configure(&config).await;
    let consent = fixture.grant(&fixture.user, first.revision).await;
    config.destination_set.insert(destination("review.example"));
    let second = fixture.configure(&config).await;
    assert!(second.revision > first.revision);
    let repo = fixture.repo();
    let resolved = effective(&repo, &fixture.user).await;
    assert_eq!(resolved.consent.as_ref(), Some(&consent));
    assert!(!resolved.inference_admitted);
    let grant = |disclosed_policy_revision| ConsentChoice::Grant {
        disclosed_policy_revision,
    };
    assert!(matches!(
        repo.set_consent(&fixture.user, consent.revision, grant(first.revision), NOW)
            .await,
        Err(StorageError::Stale)
    ));
    let renewed = repo
        .set_consent(&fixture.user, consent.revision, grant(second.revision), NOW)
        .await
        .unwrap();
    assert_eq!(renewed.revision, consent.revision + 1);
    assert_eq!(renewed.destination_set, config.destination_set);
    assert!(effective(&repo, &fixture.user).await.inference_admitted);
}

#[tokio::test]
async fn consent_revision_cas_prevents_a_late_form_from_regranting() {
    let fixture = Fixture::new().await;
    let configured = fixture.configure(&policy()).await;
    let initial = fixture.grant(&fixture.user, configured.revision).await;
    let now = NOW + TimeDelta::seconds(1);
    let repo = fixture.repo();
    let revoked = repo
        .set_consent(&fixture.user, initial.revision, ConsentChoice::Revoke, now)
        .await
        .unwrap();
    assert_eq!(revoked.revoked_at, Some(now));
    assert_eq!(revoked.revision, initial.revision + 1);
    assert!(!effective(&repo, &fixture.user).await.inference_admitted);
    let late = repo
        .set_consent(
            &fixture.user,
            initial.revision,
            ConsentChoice::Grant {
                disclosed_policy_revision: configured.revision,
            },
            now,
        )
        .await;
    assert!(matches!(late, Err(StorageError::Stale)));
    assert_eq!(repo.consent(&fixture.user).await.unwrap(), Some(revoked));
}

#[tokio::test]
async fn concurrent_first_grants_have_exactly_one_winner() {
    let fixture = Fixture::new().await;
    let configured = fixture.configure(&policy()).await;
    let repo = fixture.repo();
    let choice = ConsentChoice::Grant {
        disclosed_policy_revision: configured.revision,
    };
    let (left, right) = tokio::join!(
        repo.set_consent(&fixture.user, 0, choice, NOW),
        repo.set_consent(&fixture.user, 0, choice, NOW + TimeDelta::seconds(1)),
    );
    let winner = match (left, right) {
        (Ok(winner), Err(StorageError::Stale)) | (Err(StorageError::Stale), Ok(winner)) => winner,
        results => panic!("expected one grant and one stale writer: {results:?}"),
    };
    assert_eq!(winner.revision, 1);
    assert_eq!(repo.consent(&fixture.user).await.unwrap(), Some(winner));
}

#[tokio::test]
async fn admin_cannot_enable_another_users_mode() {
    let fixture = Fixture::new().await;
    // Prove the actor actually has account-management authority, not just a privileged-sounding name.
    AccessRepo::new(&fixture.db)
        .authorize_admin(&fixture.admin_principal)
        .await
        .unwrap();
    let configured = fixture.configure(&policy()).await;
    fixture.grant(&fixture.admin, configured.revision).await;
    fixture.grant(&fixture.user, configured.revision).await;
    let repo = fixture.repo();
    let user_before = repo.approval_mode(&fixture.user).await.unwrap();
    assert_eq!(user_before.mode, ApprovalMode::Human);
    let admin_policy = effective(&repo, &fixture.admin).await;
    let updated = repo
        .set_approval_mode(
            &fixture.admin,
            &admin_policy.version(),
            ApprovalMode::SecondaryLlm,
        )
        .await
        .unwrap();
    assert_eq!(updated.mode, ApprovalMode::SecondaryLlm);
    assert_eq!(repo.approval_mode(&fixture.admin).await.unwrap(), updated);
    assert_eq!(
        repo.approval_mode(&fixture.user).await.unwrap(),
        user_before
    );
}

#[tokio::test]
async fn approval_mode_cas_refuses_a_stale_policy_and_respects_server_ceiling() {
    let fixture = Fixture::new().await;
    let mut config = policy();
    let initial = fixture.configure(&config).await;
    fixture.grant(&fixture.user, initial.revision).await;
    let repo = fixture.repo();
    let human = effective(&repo, &fixture.user).await;
    let llm = repo
        .set_approval_mode(&fixture.user, &human.version(), ApprovalMode::SecondaryLlm)
        .await
        .unwrap();
    assert_eq!(llm.mode, ApprovalMode::SecondaryLlm);
    assert!(matches!(
        repo.set_approval_mode(&fixture.user, &human.version(), ApprovalMode::Human)
            .await,
        Err(StorageError::Stale)
    ));
    let before_ceiling_change = effective(&repo, &fixture.user).await;
    config.allow_automatic = false;
    fixture.configure(&config).await;
    assert!(matches!(
        repo.set_approval_mode(
            &fixture.user,
            &before_ceiling_change.version(),
            ApprovalMode::Human
        )
        .await,
        Err(StorageError::Stale)
    ));
    let limited = effective(&repo, &fixture.user).await;
    let disabled = repo
        .set_approval_mode(&fixture.user, &limited.version(), ApprovalMode::Human)
        .await
        .unwrap();
    assert_eq!(disabled.mode, ApprovalMode::Human);
    assert!(disabled.revision > llm.revision);
    let limited = effective(&repo, &fixture.user).await;
    assert!(
        repo.set_approval_mode(
            &fixture.user,
            &limited.version(),
            ApprovalMode::SecondaryLlm
        )
        .await
        .is_err()
    );
    assert_eq!(repo.approval_mode(&fixture.user).await.unwrap(), disabled);
}

#[tokio::test]
async fn policy_reconciliation_retains_consent_and_does_not_reuse_old_configured_revision() {
    let fixture = Fixture::new().await;
    let original_config = policy();
    let original = fixture.configure(&original_config).await;
    assert_eq!(fixture.configure(&original_config).await, original);
    let consent = fixture.grant(&fixture.user, original.revision).await;
    let mut new_config = original_config.clone();
    new_config.inference_policy_id = InferencePolicyId("b".repeat(64));
    let changed = fixture.configure(&new_config).await;
    let repo = fixture.repo();
    let resolved = effective(&repo, &fixture.user).await;
    assert!(changed.revision > original.revision);
    assert_eq!(resolved.configured, Some(changed.clone()));
    assert_eq!(resolved.consent, Some(consent.clone()));
    assert!(resolved.inference_admitted);
    assert!(
        repo.reconcile_policy(&fixture.system, None)
            .await
            .unwrap()
            .is_none()
    );
    let disabled = effective(&repo, &fixture.user).await;
    assert!(disabled.configured.is_none());
    assert_eq!(disabled.consent, Some(consent));
    assert!(!disabled.inference_admitted);
    let back = fixture.configure(&original_config).await;
    assert!(back.revision > changed.revision);
    assert!(effective(&repo, &fixture.user).await.inference_admitted);
}

#[tokio::test]
async fn suspended_owner_is_refused_on_interactive_and_scheduled_admission() {
    let fixture = Fixture::new().await;
    let configured = fixture.configure(&policy()).await;
    let granted = fixture.grant(&fixture.user, configured.revision).await;
    let repo = fixture.repo();
    let owner: &AccountId = fixture.user.account();
    let current = effective(&repo, &fixture.user).await;
    assert!(current.inference_admitted);
    assert!(
        repo.scheduled_policy(&fixture.system, owner)
            .await
            .unwrap()
            .inference_admitted
    );

    let admin = AccessRepo::new(&fixture.db)
        .authorize_admin(&fixture.admin_principal)
        .await
        .unwrap();
    AdminRepo::new(fixture.db.inner())
        .set_status(&admin, owner, true, Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        repo.effective_policy(&fixture.user).await,
        Err(StorageError::Stale)
    ));
    assert!(matches!(
        repo.scheduled_policy(&fixture.system, owner).await,
        Err(StorageError::Stale)
    ));
    assert!(matches!(
        repo.set_consent(&fixture.user, granted.revision, ConsentChoice::Revoke, NOW)
            .await,
        Err(StorageError::Stale)
    ));
    assert!(matches!(
        repo.set_approval_mode(
            &fixture.user,
            &current.version(),
            ApprovalMode::SecondaryLlm
        )
        .await,
        Err(StorageError::Stale)
    ));
}
