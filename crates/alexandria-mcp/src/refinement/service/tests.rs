use std::collections::{BTreeMap, BTreeSet};

use alexandria_engine::refinement::policy::{Protocol, Route};
use alexandria_storage::access::{AccessRepo, SystemContext};
use alexandria_storage::models::identity::OperationId;
use alexandria_storage::models::refinement::{
    ConfiguredPolicy, InferencePolicyId, Protocol as StoredProtocol, RouteDestinationId,
};
use alexandria_storage::repos::{AuthRepo, IdentityRepo, PasswordDigest, RefinementRepo};
use alexandria_storage::schema;
use chrono::Utc;
use tokio::sync::OnceCell;

use super::*;

struct Fixture {
    db: Database,
    system: SystemContext,
    alice: OwnerContext,
    bob: OwnerContext,
}

impl Fixture {
    async fn new() -> Self {
        let mut db = Database::connect_embedded().await.unwrap();
        schema::migrate(db.inner()).await.unwrap();
        let system = db.claim_system_authority().unwrap();
        let alice = owner(&db, &system, "refinement-alice").await;
        let bob = owner(&db, &system, "refinement-bob").await;
        Self {
            db,
            system,
            alice,
            bob,
        }
    }

    fn repo(&self) -> RefinementRepo<'_> {
        RefinementRepo::new(&self.db)
    }

    async fn reconcile(
        &self,
        model: &str,
        fingerprint_byte: char,
        automatic: bool,
    ) -> ReconciledRefinement {
        let routes = RoutePair::resolve(
            Route {
                protocol: Protocol::OpenAiChatCompletions,
                api_prefix: "https://inference.example/gateway/v1".into(),
                model: model.into(),
                credential_ref: Some("REFINEMENT_TEST_KEY".into()),
                headers: BTreeMap::new(),
                output_limit_field: None,
            },
            None,
        )
        .unwrap();
        let configuration = ConfiguredPolicy {
            destination_set: BTreeSet::from([RouteDestinationId {
                protocol: StoredProtocol::OpenAiChatCompletions,
                scheme: "https".into(),
                host: "inference.example".into(),
                effective_port: 443,
                api_prefix: "/gateway/v1".into(),
            }]),
            inference_policy_id: InferencePolicyId(fingerprint_byte.to_string().repeat(64)),
            allow_automatic: automatic,
        };
        let policy = self
            .repo()
            .reconcile_policy(&self.system, Some(&configuration))
            .await
            .unwrap()
            .unwrap();
        ReconciledRefinement { policy, routes }
    }

    async fn grant_directly(
        &self,
        owner: &OwnerContext,
        runtime: &ReconciledRefinement,
    ) -> Consent {
        self.repo()
            .set_consent(owner, 0, grant(runtime.policy.revision), Utc::now())
            .await
            .unwrap()
    }
}

async fn owner(db: &Database, system: &SystemContext, login: &str) -> OwnerContext {
    static DIGEST: OnceCell<PasswordDigest> = OnceCell::const_new();
    let digest = DIGEST
        .get_or_init(|| async {
            AuthRepo::create_password_digest("refinement-service-fixture-password-padding")
                .await
                .unwrap()
        })
        .await;
    let account = IdentityRepo::new(db.inner())
        .bootstrap_owner(login, digest.as_str(), &OperationId::new(login))
        .await
        .unwrap()
        .account;
    let access = AccessRepo::new(db);
    let principal = access.local_principal(system, &account).await.unwrap();
    access.authorize(&principal).await.unwrap()
}

fn grant(disclosed_policy_revision: u64) -> ConsentChoice {
    ConsentChoice::Grant {
        disclosed_policy_revision,
    }
}

#[tokio::test]
async fn consent_refuses_a_superseded_disclosure_without_granting() {
    let fixture = Fixture::new().await;
    let before = fixture.reconcile("first", 'a', false).await;
    let current = fixture.reconcile("second", 'b', false).await;
    let service = RefinementService::new(&fixture.db, Some(&current));

    let stale = service
        .consent(&fixture.alice, 0, grant(before.policy.revision))
        .await;
    assert!(
        matches!(stale, Err(ServiceError::Stale)),
        "a superseded disclosure must map to Stale: {stale:?}"
    );
    assert!(
        fixture
            .repo()
            .consent(&fixture.alice)
            .await
            .unwrap()
            .is_none()
    );
    let accepted = service
        .consent(&fixture.alice, 0, grant(current.policy.revision))
        .await
        .unwrap();
    assert_eq!(accepted.revision, 1);
    assert_eq!(&accepted.account, fixture.alice.account());
}

#[tokio::test]
async fn revoked_owner_authority_maps_to_stale_on_every_entry_point() {
    let fixture = Fixture::new().await;
    let runtime = fixture.reconcile("proposer", 'a', true).await;
    let service = RefinementService::new(&fixture.db, Some(&runtime));
    let auth = AuthRepo::new(fixture.db.inner());
    let now = Utc::now();
    let agent = auth
        .mint_agent_token(&fixture.alice, "refinement-agent", now)
        .await
        .unwrap();
    let principal = auth
        .authenticate_agent_token(agent.secret.render().expose(), now)
        .await
        .unwrap();
    let stale_owner = AccessRepo::new(&fixture.db)
        .authorize(&principal)
        .await
        .unwrap();
    let current = fixture.repo().effective_policy(&stale_owner).await.unwrap();
    auth.revoke_credential(&fixture.alice, &agent.credential, now)
        .await
        .unwrap();

    let outcomes = (
        service.effective_policy(&stale_owner).await,
        service
            .consent(&stale_owner, 0, grant(runtime.policy.revision))
            .await,
        service
            .set_approval_mode(&stale_owner, &current.version(), ApprovalMode::SecondaryLlm)
            .await,
    );
    assert!(
        matches!(
            outcomes,
            (
                Err(ServiceError::Stale),
                Err(ServiceError::Stale),
                Err(ServiceError::Stale)
            )
        ),
        "read, consent and mode must each recheck owner authority and map its revocation to Stale: {outcomes:?}"
    );
    let alice = service.effective_policy(&fixture.alice).await.unwrap();
    assert!(alice.policy.consent.is_none());
    assert_eq!(alice.approval_mode, ApprovalMode::Human);
}

#[tokio::test]
async fn approval_mode_is_account_local_and_clamped_by_operator_ceiling() {
    let fixture = Fixture::new().await;
    let runtime = fixture.reconcile("proposer", 'a', true).await;
    fixture.grant_directly(&fixture.alice, &runtime).await;
    let service = RefinementService::new(&fixture.db, Some(&runtime));
    let alice = fixture
        .repo()
        .effective_policy(&fixture.alice)
        .await
        .unwrap();
    let result = service
        .set_approval_mode(&fixture.alice, &alice.version(), ApprovalMode::SecondaryLlm)
        .await;
    assert!(
        matches!(&result, Ok(setting) if setting.mode == ApprovalMode::SecondaryLlm),
        "a valid owner may select SecondaryLlm under an automatic ceiling: {result:?}"
    );
    let bob = service.effective_policy(&fixture.bob).await.unwrap();
    assert_eq!(bob.approval_mode, ApprovalMode::Human);
    assert!(!bob.policy.inference_admitted);

    let limited = fixture.reconcile("proposer", 'a', false).await;
    let service = RefinementService::new(&fixture.db, Some(&limited));
    let alice = service.effective_policy(&fixture.alice).await.unwrap();
    assert_eq!(alice.policy.approval.mode, ApprovalMode::SecondaryLlm);
    assert_eq!(alice.approval_mode, ApprovalMode::Human);
    assert!(matches!(
        service
            .set_approval_mode(
                &fixture.alice,
                &alice.policy.version(),
                ApprovalMode::SecondaryLlm
            )
            .await,
        Err(ServiceError::Unavailable)
    ));
}

#[tokio::test]
async fn effective_policy_discloses_current_model_and_reads_after_revocation() {
    let fixture = Fixture::new().await;
    let before = fixture.reconcile("first", 'a', false).await;
    let consent = fixture.grant_directly(&fixture.alice, &before).await;
    let current = fixture.reconcile("second", 'b', false).await;
    let service = RefinementService::new(&fixture.db, Some(&current));
    let view = service.effective_policy(&fixture.alice).await;
    assert!(
        view.is_ok(),
        "an active owner can read the effective policy for disclosure: {view:?}"
    );
    let view = view.unwrap();
    assert!(view.policy.inference_admitted);
    let routes = view.routes.unwrap();
    assert_eq!(routes.proposer.model, "second");
    assert_eq!(
        routes.proposer.destination,
        before.routes.disclosure().proposer.destination
    );
    assert!(routes.reviewer_inherited && routes.same_model);

    service
        .consent(&fixture.alice, consent.revision, ConsentChoice::Revoke)
        .await
        .unwrap();
    let review = service.effective_policy(&fixture.alice).await.unwrap();
    assert!(!review.policy.inference_admitted);
    assert_eq!(review.approval_mode, ApprovalMode::Human);
    assert!(review.policy.consent.unwrap().revoked_at.is_some());
}
