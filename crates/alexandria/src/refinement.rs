//! Explicit offline refinement and policy commands for the configured trusted-local principal.

use std::{path::Path, time::Duration};

use alexandria_mcp::refinement::provider::RouteProvider;
use alexandria_mcp::refinement::worker::{Dispatch, Worker, WorkerOutput, WorkerSettings};
use alexandria_mcp::refinement::{
    EffectiveRefinementPolicy, ReconciledRefinement, RefinementService,
};
use alexandria_pipeline::embedding::load_provider;
use alexandria_storage::access::{AccessRepo, OwnerContext, Principal, SystemContext};
use alexandria_storage::models::identity::ToSql;
use alexandria_storage::models::refinement::{ApprovalMode, ConsentChoice, ProposalState};
use alexandria_storage::models::{MemoryScope, Partition, ReadSelection};
use alexandria_storage::offline_copy::OfflineStoreGuard;
use alexandria_storage::ownership_migration::check_ownership_ready;
use alexandria_storage::repos::RefinementRepo;
use alexandria_storage::repos::refinement_repo::{
    AttemptState, EnqueueRequest, WorkKind, WorkSelection,
};
use alexandria_storage::{Database, schema, system_config};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use serde_json::json;

use crate::config::{Config, reconcile_refinement_policy};

const HELP: &str = "\
Usage: alexandria refinement routes
       alexandria refinement consent grant --policy-revision N --consent-revision N
       alexandria refinement consent revoke --consent-revision N
       alexandria refinement approval human|secondary-llm

Commands act only on [auth] stdio_account. Review `refinement routes` before granting consent;
its revision fields bind the grant to the disclosed inference destinations. Commands require
exclusive access to the local store. Stop the service first. These policy commands do not run
inference; use the explicit `dream --job refine` invocation for a single partition. The offline
pass stages proposals for review; it does not apply a secondary verdict to the corpus.";

const EGRESS_DISCLOSURE: &str = "Refinement sends complete selected memory text and evidence to \
    the disclosed proposer and, when used, reviewer. This can incur provider charges, even with \
    a local route. Revocation stops future sends; it cannot retract already transmitted data.";

#[derive(Debug)]
enum Request {
    Routes,
    Consent {
        expected: u64,
        choice: ConsentChoice,
    },
    Approval(ApprovalMode),
}

fn parse(args: &[&str]) -> Result<Request> {
    if args.contains(&"--account") {
        bail!("`--account` is not accepted; commands act only on [auth] stdio_account.\n{HELP}");
    }
    match args {
        ["routes"] => Ok(Request::Routes),
        ["consent", "grant", rest @ ..] => parse_grant(rest),
        ["consent", "revoke", "--consent-revision", revision] => Ok(Request::Consent {
            expected: parse_revision(revision, "--consent-revision")?,
            choice: ConsentChoice::Revoke,
        }),
        ["approval", "human"] => Ok(Request::Approval(ApprovalMode::Human)),
        ["approval", "secondary-llm"] => Ok(Request::Approval(ApprovalMode::SecondaryLlm)),
        _ => bail!("invalid refinement command or options.\n{HELP}"),
    }
}

fn parse_grant(args: &[&str]) -> Result<Request> {
    let (policy, consent) = match args {
        ["--policy-revision", policy, "--consent-revision", consent]
        | ["--consent-revision", consent, "--policy-revision", policy] => (policy, consent),
        _ => {
            bail!("grant requires the disclosed --policy-revision and --consent-revision.\n{HELP}")
        }
    };
    Ok(Request::Consent {
        expected: parse_revision(consent, "--consent-revision")?,
        choice: ConsentChoice::Grant {
            disclosed_policy_revision: parse_revision(policy, "--policy-revision")?,
        },
    })
}

fn parse_revision(text: &str, option: &str) -> Result<u64> {
    text.parse()
        .with_context(|| format!("`{option}` must be a revision number.\n{HELP}"))
}

pub(crate) async fn run(args: &[&str]) -> Result<()> {
    if matches!(args, ["--help" | "-h"]) {
        println!("{HELP}");
        return Ok(());
    }
    let request = parse(args)?;
    let config = Config::load()?;
    let mut db = open_local_store(&config.database.data_dir).await?;
    let system = db.claim_system_authority()?;
    let (_, owner) = resolve_owner(&db, &system, &config).await?;
    let configured = reconcile_refinement_policy(&db, &system, &config.refinement).await?;
    let service = RefinementService::new(&db, configured.as_ref());
    match request {
        Request::Routes => {}
        Request::Consent { expected, choice } => {
            service.consent(&owner, expected, choice).await.context(
                "consent change refused; inspect `alexandria refinement routes` before retrying",
            )?;
        }
        Request::Approval(mode) => {
            let version = service.effective_policy(&owner).await?.policy.version();
            service
                .set_approval_mode(&owner, &version, mode)
                .await
                .context("approval mode change refused")?;
        }
    }
    print_policy(service.effective_policy(&owner).await?)
}

fn print_policy(effective: EffectiveRefinementPolicy) -> Result<()> {
    let version = effective.policy.version();
    println!(
        "{}",
        serde_json::to_string(&json!({
            "policy_revision": version.configured_revision,
            "consent_revision": version.consent_revision,
            "inference_admitted": effective.policy.inference_admitted,
            "approval_mode": effective.approval_mode,
            "routes": effective.routes,
            "disclosure": EGRESS_DISCLOSURE,
        }))?
    );
    Ok(())
}

async fn resolve_owner(
    db: &Database,
    system: &SystemContext,
    config: &Config,
) -> Result<(Principal, OwnerContext)> {
    let principal = crate::configured_local_principal(db, system, config)
        .await?
        .context("configure [auth] stdio_account with an active trusted-local account first")?;
    let owner = AccessRepo::new(db)
        .authorize(&principal)
        .await
        .context("could not authorize the configured trusted-local owner")?;
    Ok((principal, owner))
}

async fn open_local_store(path: &Path) -> Result<Database> {
    // An engine open *as a probe* would truncate even another process's LOCK. Check the
    // existing file read-only, then hand off to the engine for the actual single-writer open.
    let probe = OfflineStoreGuard::acquire(path).map_err(lock_refusal)?;
    drop(probe);
    let db = Database::connect_persistent(path)
        .await
        .map_err(lock_refusal)?;
    system_config::stored_embedding_space(db.inner()).await.context(
        "refinement refused before opening the corpus; resume an incomplete embedding target with \
         `alexandria migrate-embeddings --output-data-dir PATH --resume`",
    )?;
    check_ownership_ready(db.inner()).await.context(
        "refinement requires a ready ownership store; see `alexandria bootstrap-owner --help`",
    )?;
    schema::migrate(db.inner()).await?;
    Ok(db)
}

fn lock_refusal(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| {
        let detail = cause.to_string();
        detail.contains("Source store is locked") || detail.contains("already locked")
    }) {
        anyhow::anyhow!(
            "refinement requires a single-writer store. Stop the alexandria service or other holder, then retry; no refinement ran"
        )
    } else {
        error.context("refinement requires an existing, exclusively openable persistent store")
    }
}

pub(crate) async fn run_dream(partition: Partition) -> Result<()> {
    let config = Config::load()?;
    let mut db = open_local_store(&config.database.data_dir).await?;
    let system = db.claim_system_authority()?;
    let (principal, owner) = resolve_owner(&db, &system, &config).await?;
    authorize_partition(&db, principal, owner.clone(), &partition).await?;

    system_config::check_boot_embedding_metadata(
        db.inner(),
        &config.embedding.model,
        config.embedding.max_tokens,
    )
    .await?;
    let stored = system_config::stored_embedding_space(db.inner()).await?
        .context("explicit refinement requires an established embedding identity; boot the matching store first")?;
    let configured = reconcile_refinement_policy(&db, &system, &config.refinement).await?;
    let service = RefinementService::new(&db, configured.as_ref());
    ensure!(
        service
            .effective_policy(&owner)
            .await?
            .policy
            .inference_admitted,
        "no current consent for the configured inference destinations; inspect `alexandria refinement routes` and grant consent explicitly"
    );
    let configured = configured.context("no configured refinement routes")?;

    // Nothing in the default dream or the consent commands ever reaches this provider load.
    let settings = WorkerSettings::default();
    let loaded = tokio::time::timeout(
        Duration::from_secs(settings.pass_active_secs),
        load_provider((&config.embedding).into()),
    )
    .await
    .context("embedding provider load timed out")?
    .context("could not load the locked embedding provider")?;
    ensure!(
        loaded.space == stored,
        "loaded embedding provider identity differs from the locked store; restore the matching artifacts or migrate embeddings offline"
    );
    drop(loaded);

    run_scoped_turn(&db, &system, &configured, owner, partition, settings).await
}

async fn authorize_partition(
    db: &Database,
    principal: Principal,
    owner: OwnerContext,
    partition: &Partition,
) -> Result<()> {
    let access = AccessRepo::new(db);
    // Administrative capability is a separate gate, not an authority to adopt someone else's workspace.
    let _admin = access
        .authorize_admin(&principal)
        .await
        .context("explicit refine requires an active trusted-local administrator")?;
    ensure!(
        owner.workspace() == &partition.workspace,
        "the requested workspace is not the configured trusted-local owner's workspace"
    );
    ensure!(
        partition.scope != MemoryScope::Unassigned,
        "explicit refine requires Personal or a project scope"
    );
    access
        .select(&owner, ReadSelection::One(partition.scope.clone()))
        .await
        .context("the requested scope is not available in the trusted-local workspace")?;
    Ok(())
}

async fn run_scoped_turn(
    db: &Database,
    system: &SystemContext,
    configured: &ReconciledRefinement,
    owner: OwnerContext,
    partition: Partition,
    settings: WorkerSettings,
) -> Result<()> {
    let operation_id = format!(
        "{}-{}",
        crate::dreaming::cli_run_id(crate::dreaming::now_secs()),
        uuid::Uuid::new_v4()
    );
    let service = RefinementService::new(db, Some(configured));
    let attempt = service
        .enqueue(
            &owner,
            EnqueueRequest {
                owner: owner.account().clone(),
                partition: partition.clone(),
                kind: WorkKind::Assess,
                source_revision: None,
                selection: WorkSelection::Scheduled,
                idempotency_key: operation_id.clone(),
            },
        )
        .await
        .context("could not persist the explicit refinement attempt")?;
    let attempt_id = attempt.record().to_sql();
    // Print the discoverable, durable id before the first outbound call or an interruption.
    eprintln!("refinement operation {operation_id}; attempt {attempt_id}");

    let provider = RouteProvider::new(configured, settings);
    let worker = Worker::new(db, system, configured, settings);
    let dispatch = worker
        .step_assigned_attempt(
            owner.clone(),
            partition.clone(),
            attempt.clone(),
            Utc::now(),
            &provider,
        )
        .await
        .context("explicit refinement worker failed")?;
    let repo = RefinementRepo::new(db);
    let state = service.attempt(&owner, &attempt).await?.state;
    let (status, proposal_id, pass_id) = match (dispatch, state) {
        (
            Dispatch::Completed {
                pass,
                output: WorkerOutput::Proposal(proposal),
                ..
            },
            AttemptState::Completed,
        ) => (
            if proposal.state == ProposalState::Pending {
                "pending_review"
            } else {
                "existing_proposal"
            },
            Some(proposal.id.to_sql()),
            Some(pass.0),
        ),
        (Dispatch::Idle, AttemptState::Completed) => ("no_proposal", None, None),
        (Dispatch::Denied, AttemptState::Queued) => {
            // Keep the durable attempt, but do not leave a one-shot request armed for a future
            // background worker. A new explicit run can try again when rolling budgets permit.
            repo.cancel_queued(system, &attempt).await?;
            ("deferred", None, None)
        }
        (Dispatch::Denied, AttemptState::Failed) => ("failed", None, None),
        (Dispatch::Denied, AttemptState::Cancelled) => ("cancelled", None, None),
        _ => bail!("unexpected explicit worker state; inspect attempt {attempt_id}"),
    };
    let checkpoint = repo.candidate_checkpoint(system, &partition).await?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "job": "refine",
            "operation_id": operation_id,
            "attempt_id": attempt_id,
            "pass_id": pass_id,
            "proposal_id": proposal_id,
            "status": status,
            "has_continuation": checkpoint.position.is_some(),
        }))?
    );
    ensure!(
        status != "failed" && status != "cancelled",
        "refinement {status}; inspect attempt {attempt_id}"
    );
    Ok(())
}
