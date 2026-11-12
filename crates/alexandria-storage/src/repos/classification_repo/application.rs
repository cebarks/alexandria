//! The classification batch's capture→seal→execute path.
//!
//! Three phases, each with its own honesty rule:
//!
//! * **Capture** ([`ClassificationRepo::prepare_batch`]) resolves every cited proposal under the owner's
//!   own predicates and freezes what it found: the approval citation, the fact's whole snapshot, the
//!   source cluster's identity, and the fact's own vector. A definite refusal (absent, foreign, decided,
//!   stale) is captured as a refusal the caller can still act on; an *unexpected* error stops the whole
//!   capture, because a batch whose capture failed half-way must not execute the half that happened to
//!   succeed.
//! * **Seal** ([`ClassificationCandidates::seal`]) fixes each entry's destination membership. The caller
//!   may only choose among the captured candidates, once each; derived numbers are stripped so the
//!   sealed batch carries identity and shape — which cluster, or a mint, or grouped-by-nothing — never a
//!   population sample. The moved memory's own contribution stays pinned to the frozen fact.
//! * **Execute** ([`ClassificationRepo::apply_prepared_batch`]) runs each sealed entry through the shared
//!   [`commit_prepared`] driver: the approval is decided by a conditional update *inside* the move's own
//!   transaction, so a proposal that moved between capture and commit can be neither approved by a stale
//!   citation nor left approved while its memory stayed put.
//!
//! Every selection is accounted for exactly once. An entry is applied, refused, or — when an unexpected
//! error or an unconfirmable outcome halts the walk — named as the stopped entry with the not-yet-tried
//! entries listed after it. Those three plus the pre-capture refusals are the whole result; nothing is
//! silently dropped and no refusal is invented.

use crate::access::MutationContext;
use crate::error::StorageError;
use crate::models::cluster::mean_centroid;
use crate::models::identity::{FactId, MemoryScope, OperationId, ProjectId, Version};
use crate::models::mutation::EdgeKind;
use crate::models::mutation::{
    BatchConflict, BatchConflictReason, BatchResult, BatchStop, ClassificationApproval,
    DestinationMembership, ExpectedState, FactSnapshot, LeavingCluster, MoveReconciliation,
    PreparedChangeSet, Visibility, effect,
};
use crate::models::{proposal_state, scope_kind};
use crate::repos::classification_repo::{ClassificationRepo, ScopeProposalRef};
use crate::repos::mutation_repo::MutationRepo;
use crate::repos::mutation_retry::commit_prepared;
use crate::repos::scope::{Bind, GuardedWrite, guarded_run, record_key};
use chrono::Utc;
use surrealdb::types::{RecordId, SurrealValue};

/// A revision narrowed to the storable range, refusing rather than saturating.
///
/// A `u64` that does not fit `i64` saturated to `i64::MAX` would silently match *no* row and read as
/// "nothing decided", so every cite that reaches a `WHERE revision = $r` clause goes through here.
pub(super) fn revision_i64(revision: Version) -> Result<i64, StorageError> {
    i64::try_from(revision.get()).map_err(|_| {
        StorageError::InvalidInput("revision is out of the storable range".to_string())
    })
}

/// What one sealed entry does inside the destination partition.
///
/// Identity and shape only. A `Join` names its cluster; the guards and the mean are refreshed by
/// [`ClassificationRepo::prepare_entry`] at every attempt, because they are exactly the derived numbers a
/// retry exists to refresh. `Create` carries no centroid: the mint is centred on the frozen fact's own
/// vector, pinned at seal.
#[derive(Debug, Clone, PartialEq)]
enum SealedArrival {
    Ungrouped,
    Create,
    Join { cluster: RecordId },
}

/// One captured proposal, frozen for seal and execute.
///
/// Private because nothing outside this module may mutate a frozen opinion: the approval, the fact, the
/// source shape and the approved effect set are the batch's identity, and a setter on any of them would
/// be a second, unranked ranking path.
#[derive(Clone)]
pub(super) struct PinnedClassificationEntry {
    approval: ClassificationApproval,
    expected: ExpectedState,
    source_scope: MemoryScope,
    /// The cluster the memory was in at capture, if any. `None` means "grouped by nothing then", and a
    /// membership that appeared after capture grows the effect set and loses as `Stale` rather than
    /// being silently detached.
    source_cluster: Option<RecordId>,
    source_writes_centroid: bool,
    /// The fact's own vector at capture — the contribution a `Create` is centred on and a `Join` mean
    /// includes, pinned to the frozen fact so a retry cannot average in a vector the owner changed.
    vector: Option<Vec<f32>>,
    contributes: bool,
    /// The citation's own spelling, which the operation key, a conflict and a stop all name the entry
    /// by — the same string the reviewer handed in.
    proposal: String,
    arrival: SealedArrival,
    approved_effects: Vec<String>,
}

impl PinnedClassificationEntry {
    /// The effect labels this entry's shape always contributes, in the order `plan_move` emits them.
    ///
    /// Computed at seal so the driver's exact comparison has its fixed half: scope first, then the
    /// departure's membership and centroid, then the arrival's membership and (for a join) centroid. A
    /// same-partition, same-cluster filing emits neither membership half, and that spelling is
    /// reproduced here so it is not read as a mismatch.
    fn effect_labels(
        source_cluster: Option<&RecordId>,
        writes_centroid: bool,
        arrival: &SealedArrival,
    ) -> Vec<String> {
        let mut effects = vec![effect::SCOPE.to_string()];
        let same_cluster = match arrival {
            SealedArrival::Join { cluster } => Some(cluster) == source_cluster,
            _ => false,
        };
        if same_cluster {
            return effects;
        }
        if source_cluster.is_some() {
            effects.push(EdgeKind::ClusterMembership.effect());
            if writes_centroid {
                effects.push(effect::CLUSTER_CENTROID.to_string());
            }
        }
        match arrival {
            SealedArrival::Ungrouped => {}
            SealedArrival::Create => {
                effects.push(EdgeKind::ClusterMembership.effect());
            }
            SealedArrival::Join { .. } => {
                effects.push(EdgeKind::ClusterMembership.effect());
                effects.push(effect::CLUSTER_CENTROID.to_string());
            }
        }
        effects
    }
}

/// One captured candidate, as the ranking caller sees it.
///
/// Getters only: the citation it was captured by, the frozen fact, and the opinion's destination. The
/// caller ranks on these and hands back a [`ClassificationDestination`] per citation — it cannot reach
/// the entry's guards.
#[derive(Clone)]
pub struct ClassificationCandidate {
    reference: ScopeProposalRef,
    fact: FactSnapshot,
    destination: MemoryScope,
}

impl ClassificationCandidate {
    /// The citation this candidate was captured by: the proposal's handle and revision.
    pub fn reference(&self) -> &ScopeProposalRef {
        &self.reference
    }

    /// The fact exactly as capture read it: content, vector, visibility, relations, revision.
    pub fn fact(&self) -> &FactSnapshot {
        &self.fact
    }

    /// The placement opinion the proposal carries.
    pub fn destination(&self) -> &MemoryScope {
        &self.destination
    }
}

/// Every lawful candidate a capture produced, plus what capture already knows cannot run.
///
/// `candidates()` is empty when capture stopped: nothing executes, and the refusals captured before the
/// failure are still reported so every selection stays accounted for.
#[derive(Clone)]
struct CapturedOwner {
    account: RecordId,
    workspace: RecordId,
}

#[derive(Clone)]
pub struct ClassificationCandidates {
    owner: CapturedOwner,
    views: Vec<ClassificationCandidate>,
    entries: Vec<PinnedClassificationEntry>,
    refusals: Vec<BatchConflict>,
    /// Set when capture stopped early: the citation that failed, and every captured candidate's
    /// citation — none of which may execute, even the ones captured before the failure.
    stopped: Option<(String, Vec<String>)>,
}

impl ClassificationCandidates {
    /// The captured candidates, in citation order. Empty when capture stopped.
    pub fn candidates(&self) -> &[ClassificationCandidate] {
        &self.views
    }

    /// Fix every candidate's destination membership.
    ///
    /// Each destination must name a captured candidate, exactly once; a foreign or duplicated choice is
    /// refused. A `Refused` becomes a recorded conflict; a `Chosen` becomes a sealed entry whose derived
    /// numbers are stripped (a join keeps its cluster's identity, never its guards or mean; a mint is
    /// centred on the frozen fact's own vector). An `Unconfirmed` stops the seal — and like a capture
    /// stop, nothing sealed before it executes either, because a batch whose ranking could not be
    /// completed must not run the part that happened to finish. Missing destinations are lawful only
    /// after an `Unconfirmed`; the candidates after it are unattempted.
    pub fn seal(
        self,
        destinations: &[ClassificationDestination],
    ) -> Result<PreparedClassificationBatch, StorageError> {
        let ClassificationCandidates {
            owner,
            views,
            entries,
            refusals,
            stopped,
        } = self;
        if let Some((proposal, unattempted)) = stopped {
            return Ok(PreparedClassificationBatch {
                owner,
                entries: Vec::new(),
                refusals,
                stopped: Some(BatchStop {
                    proposal,
                    unattempted,
                }),
            });
        }
        // Duplicated and foreign choices are refused before anything is sealed.
        let mut cited: Vec<&str> = Vec::new();
        for destination in destinations {
            let id = destination_reference(destination).id.as_str();
            if cited.contains(&id) {
                return Err(StorageError::InvalidInput(
                    "one captured candidate was given two destinations".to_string(),
                ));
            }
            if !views
                .iter()
                .any(|view| &view.reference == destination_reference(destination))
            {
                return Err(StorageError::InvalidInput(
                    "a destination named a proposal this batch did not capture".to_string(),
                ));
            }
            cited.push(id);
        }
        let mut sealed: Vec<PinnedClassificationEntry> = Vec::new();
        let mut conflicts = refusals;
        let mut stop: Option<BatchStop> = None;
        for (index, entry) in entries.into_iter().enumerate() {
            let fact = &views[index].fact;
            if let Some(stop) = &mut stop {
                // After an Unconfirmed only the missing are lawful: a choice about a candidate the
                // seal already declared unattempted is refused, not silently ignored.
                if destinations
                    .iter()
                    .any(|destination| destination_reference(destination).id == entry.proposal)
                {
                    return Err(StorageError::InvalidInput(
                        "a destination named a candidate after the batch stopped".to_string(),
                    ));
                }
                stop.unattempted.push(entry.proposal.clone());
                continue;
            }
            let chosen = destinations
                .iter()
                .find(|destination| destination_reference(destination).id == entry.proposal);
            let Some(chosen) = chosen else {
                return Err(StorageError::InvalidInput(
                    "a captured candidate was given no destination".to_string(),
                ));
            };
            match chosen {
                ClassificationDestination::Refused { reason, .. } => {
                    conflicts.push(BatchConflict {
                        proposal: entry.proposal.clone(),
                        reason: *reason,
                    });
                }
                ClassificationDestination::Unconfirmed { .. } => {
                    stop = Some(BatchStop {
                        proposal: entry.proposal.clone(),
                        unattempted: Vec::new(),
                    });
                }
                ClassificationDestination::Chosen { arriving, .. } => {
                    let arrival = match arriving {
                        DestinationMembership::Unclustered => SealedArrival::Ungrouped,
                        DestinationMembership::Create { .. } => {
                            if fact.embedding.is_none() {
                                return Err(StorageError::InvalidInput(
                                    "a minted destination cluster needs the moved memory's vector"
                                        .to_string(),
                                ));
                            }
                            SealedArrival::Create
                        }
                        DestinationMembership::Join { expectation, .. } => SealedArrival::Join {
                            cluster: expectation.cluster.clone(),
                        },
                    };
                    let approved_effects = PinnedClassificationEntry::effect_labels(
                        entry.source_cluster.as_ref(),
                        entry.source_writes_centroid,
                        &arrival,
                    );
                    sealed.push(PinnedClassificationEntry {
                        arrival,
                        approved_effects,
                        ..entry
                    });
                }
            }
        }
        if let Some(stop) = stop {
            // Nothing executes: the candidates sealed before the stop are as unattempted as the ones
            // after it, and the stop's own entry is neither refused nor retried by this batch.
            let unattempted = sealed
                .iter()
                .map(|entry| entry.proposal.clone())
                .chain(stop.unattempted.iter().cloned())
                .collect();
            return Ok(PreparedClassificationBatch {
                entries: Vec::new(),
                owner,
                refusals: conflicts,
                stopped: Some(BatchStop {
                    unattempted,
                    ..stop
                }),
            });
        }
        Ok(PreparedClassificationBatch {
            owner,
            entries: sealed,
            refusals: conflicts,
            stopped: None,
        })
    }
}

/// The caller's sealed decision for one captured candidate.
///
/// A closed, trusted Rust enum: `Chosen` names the destination membership, `Refused` a definite refusal
/// with a reason the owner can act on, `Unconfirmed` an outcome that could not be established and must
/// not be guessed. Seal stops at an `Unconfirmed`: the entries after it are not attempted.
#[derive(Debug, Clone, PartialEq)]
pub enum ClassificationDestination {
    Chosen {
        reference: ScopeProposalRef,
        arriving: DestinationMembership,
    },
    Refused {
        reference: ScopeProposalRef,
        reason: BatchConflictReason,
    },
    Unconfirmed {
        reference: ScopeProposalRef,
    },
}

fn destination_reference(destination: &ClassificationDestination) -> &ScopeProposalRef {
    match destination {
        ClassificationDestination::Chosen { reference, .. }
        | ClassificationDestination::Refused { reference, .. }
        | ClassificationDestination::Unconfirmed { reference } => reference,
    }
}

/// A sealed batch: frozen entries, captured refusals, and at most one stop.
///
/// Executable exactly through [`ClassificationRepo::apply_prepared_batch`]; there is no other way to run
/// a captured opinion and no way to edit one once sealed.
#[derive(Clone)]
pub struct PreparedClassificationBatch {
    owner: CapturedOwner,
    pub(super) entries: Vec<PinnedClassificationEntry>,
    pub(super) refusals: Vec<BatchConflict>,
    pub(super) stopped: Option<BatchStop>,
}

/// How one citation resolved at capture.
enum CaptureOutcome {
    Entry(Box<(PinnedClassificationEntry, FactSnapshot)>),
    Refusal(BatchConflictReason),
    Stop,
}

impl ClassificationRepo<'_> {
    /// Capture the cited proposals: resolve, validate, freeze.
    ///
    /// Duplicate citations and unrepresentable revisions are refused before anything is read, so a
    /// malformed batch cannot leave half a capture behind. Each remaining citation is resolved under the
    /// owner's own predicates; a definite refusal is captured, an unexpected error stops the capture
    /// (and nothing captured executes), and a lawful citation becomes a candidate carrying its frozen
    /// fact and the source shape the move must preserve.
    pub async fn prepare_batch(
        &self,
        references: &[ScopeProposalRef],
    ) -> Result<ClassificationCandidates, StorageError> {
        let mut seen: Vec<&str> = Vec::new();
        for reference in references {
            if seen.contains(&reference.id.as_str()) {
                return Err(StorageError::InvalidInput(
                    "one proposal was cited more than once in this batch".to_string(),
                ));
            }
            seen.push(reference.id.as_str());
            revision_i64(reference.revision)?;
        }
        let owner = CapturedOwner {
            account: self.account(),
            workspace: self.workspace(),
        };
        let mut views: Vec<ClassificationCandidate> = Vec::new();
        let mut entries: Vec<PinnedClassificationEntry> = Vec::new();
        let mut refusals: Vec<BatchConflict> = Vec::new();
        for (index, reference) in references.iter().enumerate() {
            match self.capture_entry(reference).await {
                CaptureOutcome::Entry(captured) => {
                    let (entry, fact) = *captured;
                    views.push(ClassificationCandidate {
                        destination: entry.approval.destination.clone(),
                        fact,
                        reference: reference.clone(),
                    });
                    entries.push(entry);
                }
                CaptureOutcome::Refusal(reason) => refusals.push(BatchConflict {
                    proposal: reference.id.clone(),
                    reason,
                }),
                CaptureOutcome::Stop => {
                    // Nothing captured may execute, but every selection is still accounted for: the
                    // refusals above stay refusals, and the captured candidates become unattempted.
                    return Ok(ClassificationCandidates {
                        owner,
                        views: Vec::new(),
                        entries: Vec::new(),
                        refusals,
                        stopped: Some((
                            reference.id.clone(),
                            views
                                .iter()
                                .map(|view| view.reference.id.clone())
                                .chain(
                                    references[index + 1..]
                                        .iter()
                                        .map(|reference| reference.id.clone()),
                                )
                                .collect(),
                        )),
                    });
                }
            }
        }
        Ok(ClassificationCandidates {
            owner,
            views,
            entries,
            refusals,
            stopped: None,
        })
    }

    /// Resolve one citation into a frozen entry, or the definite reason it cannot become one.
    async fn capture_entry(&self, reference: &ScopeProposalRef) -> CaptureOutcome {
        use CaptureOutcome::*;
        // Malformed or wrong-table citations are refused, not probed: the same answer an absent row
        // gives, so the batch cannot be used to ask what exists.
        let Ok(target) = RecordId::parse_simple(&reference.id) else {
            return Refusal(BatchConflictReason::NotFound);
        };
        if target.table.as_str() != "classification_proposal" {
            return Refusal(BatchConflictReason::NotFound);
        }
        let row = match self.load(reference).await {
            Ok(row) => row,
            Err(_) => return Stop,
        };
        let Some(row) = row else {
            return Refusal(BatchConflictReason::NotFound);
        };
        if row.state != proposal_state::PENDING {
            return Refusal(BatchConflictReason::Decided);
        }
        let Some(fact_row) = row.fact.clone() else {
            return Refusal(BatchConflictReason::NotFound);
        };
        let Ok(fact) = FactId::try_from(fact_row) else {
            return Refusal(BatchConflictReason::NotFound);
        };
        // Distinct opinions retain independent expectations. A successful earlier move stales the
        // later opinion at execution; a refused earlier opinion must not suppress a valid later one.
        let destination = match row.destination_kind.as_str() {
            scope_kind::PERSONAL => MemoryScope::Personal,
            scope_kind::UNASSIGNED => MemoryScope::Unassigned,
            scope_kind::PROJECT => match row
                .destination_project
                .clone()
                .and_then(|id| ProjectId::try_from(id).ok())
            {
                Some(project) => MemoryScope::Project(project),
                None => return Refusal(BatchConflictReason::InvalidDestination),
            },
            _ => return Refusal(BatchConflictReason::InvalidDestination),
        };
        // An unknown source revision is refused rather than guessed as zero: v009 stores it as
        // `option<int>` precisely so "formed before revisions existed" stays distinguishable.
        let Some(source) = row
            .source_revision
            .and_then(|value| u64::try_from(value).ok())
        else {
            return Refusal(BatchConflictReason::Stale);
        };
        let source_revision = Version::new(source);
        let snapshot = match MutationRepo::new(self.db).state(self.owner, &fact).await {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Refusal(BatchConflictReason::NotFound),
            Err(_) => return Stop,
        };
        if snapshot.version != source_revision {
            // The fact is *positively known* to have moved past the cited revision — this read just
            // said so — so the still-pending opinion is retired as stale here, at capture: the queue
            // must not offer again a judgement whose subject has already moved. An unexpected
            // retirement failure stops the capture rather than leaving a half-accounted batch.
            if let Some(handle) = row.id.clone() {
                let approval = ClassificationApproval {
                    proposal: handle,
                    proposal_revision: reference.revision,
                    fact: fact.clone(),
                    source_revision,
                    destination,
                };
                match self.retire_stale_source(&approval).await {
                    Ok(()) | Err(StorageError::Stale) | Err(StorageError::NotFound) => {}
                    Err(_) => return Stop,
                }
            }
            return Refusal(BatchConflictReason::Stale);
        }
        // The source cluster's identity, frozen. A membership read, not a ranking: only the edge can
        // say which cluster's stored centre a departure makes wrong.
        let (source_cluster, source_writes_centroid) = match MutationRepo::new(self.db)
            .cluster_of(self.owner, fact.as_record_id())
            .await
        {
            Ok(Some((cluster, expectation))) => {
                let survivors: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .filter(|member| &member.id != fact.as_record_id())
                    .map(|member| member.embedding.as_slice())
                    .collect();
                let mean = mean_centroid(&survivors);
                if !survivors.is_empty() && mean.is_none() {
                    return Refusal(BatchConflictReason::InvalidRequest);
                }
                (Some(cluster), mean.is_some())
            }
            Ok(None) => (None, false),
            Err(_) => return Stop,
        };
        let Some(handle) = row.id.clone() else {
            return Refusal(BatchConflictReason::NotFound);
        };
        Entry(Box::new((
            PinnedClassificationEntry {
                approval: ClassificationApproval {
                    proposal: handle,
                    proposal_revision: reference.revision,
                    fact: fact.clone(),
                    source_revision,
                    destination,
                },
                expected: ExpectedState::of(&snapshot),
                source_scope: snapshot.scope.clone(),
                source_cluster,
                source_writes_centroid,
                vector: snapshot
                    .embedding
                    .as_ref()
                    .map(|embedding| embedding.vector.clone()),
                proposal: reference.id.clone(),
                contributes: snapshot.visibility == Visibility::live(),
                arrival: SealedArrival::Ungrouped,
                approved_effects: Vec::new(),
            },
            snapshot,
        )))
    }

    /// Execute a sealed batch.
    ///
    /// Each entry goes through the shared [`commit_prepared`] driver under its own
    /// `<batch-operation>:<proposal>` key, so a prepared payload cannot re-land a committed move and a retry
    /// refreshes only the derived numbers behind the frozen effect set. This is not unconditional sealed-batch
    /// recovery: fallible preparation runs BEFORE the kernel's replay lookup. Later changes to a named cluster
    /// or its population can refuse re-execution even when that entry already has a committed receipt. Such a
    /// refusal writes nothing again; the original owner-qualified operation remains available in history.
    /// The approval is decided inside
    /// the move's transaction; an entry whose citation lost (fact moved, proposal revised, decision
    /// already taken) is a definite refusal, and — when its fact is *positively known* to have moved —
    /// its still-pending opinion is retired as stale by [`Self::retire_stale_source`]. An unexpected
    /// error halts the walk: the entry is `Unconfirmed`, the rest are unattempted, and the entries that
    /// already committed keep their result.
    pub async fn apply_prepared_batch(
        &self,
        batch: &PreparedClassificationBatch,
        operation: &OperationId,
    ) -> Result<BatchResult, StorageError> {
        if batch.owner.account != self.account() || batch.owner.workspace != self.workspace() {
            return Err(StorageError::InvalidInput(
                "the batch belongs to a different owner".into(),
            ));
        }
        let mut result = BatchResult {
            applied: Vec::new(),
            conflicts: batch.refusals.clone(),
            stopped: batch.stopped.clone(),
        };
        if result.stopped.is_some() {
            return Ok(result);
        }
        let mutation = MutationRepo::new(self.db);
        let access = MutationContext::issued_to(self.owner);
        for (index, entry) in batch.entries.iter().enumerate() {
            let key = OperationId::new(format!("{}:{}", operation.as_str(), entry.proposal));
            let outcome = commit_prepared(
                &mutation,
                &access,
                &key,
                std::slice::from_ref(&entry.expected),
                &entry.approved_effects,
                || async { self.prepare_entry(entry).await },
            )
            .await;
            match outcome {
                Ok(applied) => result.applied.push(applied.operation),
                Err(StorageError::Stale) => {
                    // Classify the slot only after retirement's outcome is known: an unexpected
                    // metadata failure makes it unconfirmed, never both refused and stopped.
                    match self.retire_stale_source(&entry.approval).await {
                        Ok(()) | Err(StorageError::Stale) | Err(StorageError::NotFound) => {}
                        Err(_) => {
                            result.stopped = Some(stop_from(&batch.entries[index..]));
                            return Ok(result);
                        }
                    }
                    result.conflicts.push(BatchConflict {
                        proposal: entry.proposal.clone(),
                        reason: BatchConflictReason::Stale,
                    });
                }
                Err(StorageError::NotFound) => result.conflicts.push(BatchConflict {
                    proposal: entry.proposal.clone(),
                    reason: BatchConflictReason::NotFound,
                }),
                Err(StorageError::InvalidInput(_)) => result.conflicts.push(BatchConflict {
                    proposal: entry.proposal.clone(),
                    reason: BatchConflictReason::InvalidRequest,
                }),
                Err(StorageError::RetryableBookkeepingConflict) => {
                    result.conflicts.push(BatchConflict {
                        proposal: entry.proposal.clone(),
                        reason: BatchConflictReason::RetryExhausted,
                    });
                }
                // Everything else is unexpected: the entry is unconfirmed, the rest unattempted, and
                // the entries that already applied keep their committed result.
                Err(_) => {
                    result.stopped = Some(stop_from(&batch.entries[index..]));
                    return Ok(result);
                }
            }
        }
        Ok(result)
    }

    /// Refresh only the derived numbers of one frozen entry: the named source cluster's guards and
    /// survivors' mean, and the named destination cluster's guards and mean.
    ///
    /// The identities never move — the source cluster, the destination cluster, the fact, the citation
    /// and the effect set are the frozen half — so a membership that changed *which* cluster is involved
    /// shows up as an effect-set mismatch in the driver and loses as `Stale`, never as a quietly
    /// different move. The moved memory's own contribution stays pinned to the frozen vector.
    async fn prepare_entry(
        &self,
        entry: &PinnedClassificationEntry,
    ) -> Result<PreparedChangeSet, StorageError> {
        let mutation = MutationRepo::new(self.db);
        let leaving = match &entry.source_cluster {
            Some(cluster) => {
                let expectation = mutation.cluster_expectation(self.owner, cluster).await?;
                let survivors: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .filter(|member| &member.id != entry.approval.fact.as_record_id())
                    .map(|member| member.embedding.as_slice())
                    .collect();
                let surviving_centroid = mean_centroid(&survivors);
                Some(LeavingCluster {
                    expectation,
                    surviving_centroid,
                })
            }
            None => None,
        };
        let arriving = match &entry.arrival {
            SealedArrival::Ungrouped => DestinationMembership::Unclustered,
            SealedArrival::Create => DestinationMembership::Create {
                centroid: entry.vector.clone().ok_or_else(|| {
                    StorageError::InvalidInput(
                        "a minted destination cluster needs the moved memory's own vector"
                            .to_string(),
                    )
                })?,
            },
            SealedArrival::Join { cluster } => {
                let expectation = mutation.cluster_expectation(self.owner, cluster).await?;
                let mut held: Vec<&[f32]> = expectation
                    .population
                    .iter()
                    .map(|member| member.embedding.as_slice())
                    .collect();
                if entry.contributes
                    && let Some(vector) = &entry.vector
                {
                    held.push(vector.as_slice());
                }
                let centroid = mean_centroid(&held).ok_or_else(|| {
                    StorageError::InvalidInput(
                        "a destination cluster's members carry no common dimension".to_string(),
                    )
                })?;
                DestinationMembership::Join {
                    expectation,
                    centroid,
                }
            }
        };
        let change = mutation
            .plan_move(
                self.owner,
                &entry.approval.fact,
                entry.approval.source_revision,
                &entry.source_scope,
                &entry.approval.destination,
                &MoveReconciliation { leaving, arriving },
            )
            .await?;
        Ok(change.approve_classification(entry.approval.clone()))
    }

    /// Retire a still-pending opinion whose fact is *positively known* to have moved.
    ///
    /// Retirement requires the exact cited opinion — same fact, same source revision, same destination,
    /// still pending, still at the cited revision — plus a current fact revision that is positively
    /// known and different. An absent fact, an unknown (`NONE`) revision, or an unchanged revision is
    /// not proof of staleness and retires nothing. A newer proposal revision or an existing decision is
    /// a no-op: the winner keeps its state. This is metadata only; no corpus journal row is written.
    async fn retire_stale_source(
        &self,
        approval: &ClassificationApproval,
    ) -> Result<(), StorageError> {
        // Positively-known current revision, read owner-qualified: `None` (absent, foreign, or a row
        // that predates the column) is not proof, and retires nothing.
        let mut response = self
            .db
            .query(
                "SELECT VALUE semantic_revision FROM type::record('fact', $key) WHERE id = $fact \
                 AND account = $account AND workspace = $workspace;",
            )
            .bind(("key", record_key(approval.fact.as_record_id())?))
            .bind(("fact", approval.fact.as_record_id().clone()))
            .bind(("account", self.account()))
            .bind(("workspace", self.workspace()))
            .await
            .map_err(super::internal)?
            .check()
            .map_err(super::internal)?;
        let current: Option<i64> = response.take(0).map_err(super::internal)?;
        let Some(current) = current else {
            return Ok(());
        };
        let cited = revision_i64(approval.source_revision)?;
        if current == cited {
            return Ok(());
        }
        let (kind, project) = match &approval.destination {
            MemoryScope::Personal => (scope_kind::PERSONAL, None),
            MemoryScope::Unassigned => (scope_kind::UNASSIGNED, None),
            MemoryScope::Project(project) => {
                (scope_kind::PROJECT, Some(project.as_record_id().clone()))
            }
        };
        // One guarded conditional decision, owner-qualified, matching the whole citation: a row that
        // moved under it keeps its newer state, and a no-match is a no-op rather than an error.
        let rows = guarded_run::<ProposalRetiredRow>(
            self.db,
            self.owner,
            &GuardedWrite::new(
                "UPDATE type::record('classification_proposal', $key) SET state = 'stale', \
                 decided_at = time::now(), decided_by = $account WHERE id = $proposal AND account = \
                 $account AND workspace = $workspace AND revision = $revision AND state = $pending \
                 AND fact = $fact AND source_revision = $source_revision AND destination_kind = \
                 $destination_kind AND destination_project = $destination_project RETURN AFTER;",
            )
            .id("proposal", approval.proposal.clone())
            .value("key", Bind::Text(record_key(&approval.proposal)?))
            .value(
                "revision",
                Bind::OptInt(Some(revision_i64(approval.proposal_revision)?)),
            )
            .value("pending", Bind::Text(proposal_state::PENDING.to_string()))
            .value(
                "fact",
                Bind::OptRecord(Some(approval.fact.as_record_id().clone())),
            )
            .value("source_revision", Bind::OptInt(Some(cited)))
            .value("destination_kind", Bind::Text(kind.to_string()))
            .value("destination_project", Bind::OptRecord(project)),
            Utc::now(),
        )
        .await?;
        // `None` is the guard loop's own refusal (authority moved mid-write), and an empty match is a
        // no-op: either way the refusal recorded by the caller is the whole answer.
        let _ = rows;
        Ok(())
    }

    /// Prepare, seal with every arrival explicitly ungrouped, and execute — the one-call form.
    pub async fn apply_batch(
        &self,
        references: &[ScopeProposalRef],
        operation: &OperationId,
    ) -> Result<BatchResult, StorageError> {
        let candidates = self.prepare_batch(references).await?;
        // Every arrival stated explicitly as ungrouped — the honest storage-only answer — rather than
        // omitted: a seal with no destinations would be a batch that ranked nothing, not one that
        // ranked every arrival into no cluster.
        let destinations: Vec<ClassificationDestination> = candidates
            .candidates()
            .iter()
            .map(|candidate| ClassificationDestination::Chosen {
                reference: candidate.reference().clone(),
                arriving: DestinationMembership::Unclustered,
            })
            .collect();
        let batch = candidates.seal(&destinations)?;
        self.apply_prepared_batch(&batch, operation).await
    }
}

/// The stop record for a halted walk: the halted entry plus every not-yet-tried one after it.
fn stop_from(entries: &[PinnedClassificationEntry]) -> BatchStop {
    BatchStop {
        proposal: entries[0].proposal.clone(),
        unattempted: entries[1..]
            .iter()
            .map(|entry| entry.proposal.clone())
            .collect(),
    }
}

/// The row a stale retirement matched, read back only to know it matched.
#[derive(SurrealValue)]
struct ProposalRetiredRow {
    #[allow(dead_code)]
    id: Option<RecordId>,
}
