-- v009: accounts, private workspaces, project scope, credentials, ownership-migration state,
--        private classification proposals, the shared semantic journal, reminder migration holds
--        and system maintenance progress.
--
-- This migration is the ownership *container*. It defines the shapes Tasks 4/6/8/10/11/12/14/15
-- write through, and it decides nothing about who owns what is already in a store.
--
--   * Every ownership/scope column added to an existing table is `option<>` with no owner default.
--     Absent means migration-pending — never Personal, never public, never the first account to
--     exist. `missing_ownership_never_means_personal` and
--     `v009_preserves_legacy_write_shapes_without_inventing_ownership` pin both halves.
--   * Revision and hold columns carry a `DEFAULT` for rows written from now on, and NO statement
--     here rewrites a row that already exists. Both halves matter on this engine: a `DEFAULT` is
--     applied at CREATE only, so declaring a revision column as a bare non-optional `int DEFAULT 0`
--     makes every later `UPDATE` of a pre-v009 row fail ("Expected `int` but found `NONE`", measured
--     on the pinned 3.3.0 engine) — which is precisely "existing legacy writes fail merely because
--     v009 was applied". They are therefore `option<…> DEFAULT …`: a new row reads 0/false, a legacy
--     row reads NONE, and every reader must treat NONE as the same safe value it would default to.
--     Turning NONE into a stored value is Task 14's guarded, backup-requiring, resumable backfill.
--     The read consequence is spelled out for Tasks 6/12: the *held* population is
--     `migration_hold = true` (or `IS NOT NONE AND …` is not it — `= NONE` matches absent rows), and
--     the deliverable population is `migration_hold != true`, because `= false` silently misses
--     every row that predates this file.
--   * New tables keep strict constraints. The legacy-compatibility rule protects rows that predate
--     this file; it is not a licence to weaken a table that did not exist before it.
--
-- Spellings, one concept each. `account` is the canonical owner column on a resource and
-- `workspace.owner` is the account that owns a workspace; the brief names the reminder's owner and
-- the session's account, and both are this same `record<account>` — `account` is used everywhere a
-- reader can see it, and the reminder's snapshot copy is `reminder_account` for the same reason.
-- Relation tables (`memory_edge`, `contains_*`, `extracted_from`, `has_provenance`) and `heat_state`
-- deliberately carry no owner column at all: the design forbids "independently writable owner
-- fields whose values can disagree", and their ownership is the uniquely validated path through
-- their endpoints, which Task 6 checks on both ends of every scoped write.
--
-- Nothing here relies on SurrealDB `PERMISSIONS` for row privacy: an embedded root session is not
-- restricted by them (measured on 3.3.0 — a `PERMISSIONS FOR create WHERE false` table still
-- accepted the write). The boundary is the storage API; these definitions are defence in depth,
-- exactly as the source design says ("schema constraints provide additional protection, but do not
-- assume SurrealDB embedded owner privileges enforce application row permissions automatically").
--
-- Two index-shape rules, both measured here rather than assumed:
--   * No UNIQUE index is declared over an `array` column that a reader scans by. A multi-key index
--     returns one row per matching element, so scanning a project by a workspace-qualified index
--     over an alias *array* double-counts the same project (3.3.0), which would corrupt every count
--     and page. Aliases are therefore workspace-qualified scalar columns / one row per alias.
--   * A cross-field `ASSERT` reads its sibling through `$this.<field>`. A bare `$scope` and
--     `$parent.scope` both evaluate to NONE at assert time on this engine, so an assertion written
--     that way rejects every row — including the valid ones.
--
-- Every statement is `DEFINE <kind> OVERWRITE` (removals would be `REMOVE <kind> IF EXISTS`)
-- because `migrate()` is not atomic across a file: each statement commits on its own and
-- `system_config.schema_version` is stamped only once the whole file succeeds, so a crash mid-file
-- leaves these definitions applied with an old stamp and the next boot re-runs this file from the
-- top. `test_replaying_a_completed_migration_is_not_an_error` is the gate on that rule. Note the
-- 3.2/3.3 grammar puts the qualifier *after* the kind: `DEFINE OVERWRITE FIELD` is a parse error.

-- ---------------------------------------------------------------------------------------------
-- Accounts and the one private workspace each owns
-- ---------------------------------------------------------------------------------------------

DEFINE TABLE OVERWRITE account SCHEMAFULL;
-- A login is how a human types their way in; it is not a key for data ownership — every ownership
-- path below goes through `record<account>` ids. It is still unique and stored normalised, because
-- `Alice` and `alice` as two accounts would be two owners that no later read can tell apart. The
-- assertion is the schema's copy of `models::auth::normalize_login`, which is what
-- `IdentityRepo::bootstrap_owner` and Task 4's credential lookups apply, so a row that somehow
-- escaped the writer still fails here rather than living beside its lookalike.
DEFINE FIELD OVERWRITE login          ON account TYPE string ASSERT $value != NONE AND $value = string::lowercase(string::trim($value)) AND string::len($value) > 2 AND string::len($value) <= 64;
-- Task 4 owns Argon2id hashing and every write of this column; Task 3 stores the string it is
-- handed. `NONE` is representable because the design separates account identity from credentials
-- for a future SSO link — an account with no password is inert for password login, not open.
DEFINE FIELD OVERWRITE password_hash  ON account TYPE option<string> ASSERT $value = NONE OR string::len(string::trim($value)) > 0;
-- `suspended` stops reads, writes and scheduled work; Task 9's admin surface is its only writer.
-- Nothing is deleted: a suspended account keeps its corpus.
DEFINE FIELD OVERWRITE status         ON account TYPE string DEFAULT 'active' ASSERT $value IN ['active', 'suspended'];
-- `admin` is an *account-management* role. It grants no read of anybody else's workspace, which is
-- what `admin_does_not_get_other_workspace` proves at the authorization seam instead of trusting
-- what a flag's name suggests.
DEFINE FIELD OVERWRITE role           ON account TYPE string DEFAULT 'owner' ASSERT $value IN ['owner', 'admin'];
-- Recovery bumps this; every credential carries the epoch it was minted under. Comparing the two is
-- what makes "invalidate all previous sessions and agent tokens" one integer write rather than a
-- best-effort sweep over live rows — and `AccessRepo::authorize` re-reads it on every call, so an
-- already-issued principal cannot outrun a recovery.
DEFINE FIELD OVERWRITE auth_epoch     ON account TYPE int DEFAULT 1 ASSERT $value >= 1;
-- The `OperationId` that created this owner: the retry anchor that makes `bootstrap_owner`
-- idempotent, and the reason a second bootstrap under a different key cannot silently reown a login.
DEFINE FIELD OVERWRITE bootstrap_operation ON account TYPE option<string>;
-- The fingerprint of the enrollment *request* that key was used for: a SHA-256 over the canonical
-- normalised login and the exact prepared `password_hash` string the request carried. The operation
-- key alone cannot prove a retry is the same request, and comparing the current `password_hash`
-- cannot either — Task 4 rotates that on recovery, and a legitimate rotation must not redefine the
-- original request. Note what this deliberately does NOT claim: hashing a salted Argon2id digest
-- does not make two independently prepared hashes of one password equal, so a retry reuses its
-- operation's prepared hash rather than asserting equivalence across salts. Absent means the row
-- predates this column — unknown history, which `bootstrap_owner` refuses instead of adopting.
DEFINE FIELD OVERWRITE bootstrap_fingerprint ON account TYPE option<string> ASSERT $value = NONE OR (string::len($value) = 64 AND $value = string::lowercase($value));
DEFINE FIELD OVERWRITE created_at     ON account TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE updated_at     ON account TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_account_login ON account FIELDS login UNIQUE;
DEFINE INDEX OVERWRITE idx_account_status ON account FIELDS status;

-- Every account owns exactly one private workspace, and `owner` being one required
-- `record<account>` with a UNIQUE index is the whole privacy model: there is no visibility column
-- to mis-set and no shared-workspace shape to fall into, because sharing is a later design
-- (memberships and grants), not a default.
DEFINE TABLE OVERWRITE workspace SCHEMAFULL;
DEFINE FIELD OVERWRITE owner      ON workspace TYPE record<account> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE created_at ON workspace TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE updated_at ON workspace TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_workspace_owner ON workspace FIELDS owner UNIQUE;

-- ---------------------------------------------------------------------------------------------
-- Projects: identity is workspace-qualified, so two accounts' `alexandria` share nothing
-- ---------------------------------------------------------------------------------------------

DEFINE TABLE OVERWRITE project SCHEMAFULL;
DEFINE FIELD OVERWRITE workspace        ON project TYPE record<workspace> ASSERT $value != NONE;
-- The primary normalized identity alias. Task 8 owns the normalization algorithm; this schema owns
-- the rule that one normalized alias cannot name two projects in one workspace, and that the same
-- alias can name one project in each of two workspaces (`same_alias_in_two_workspaces`).
DEFINE FIELD OVERWRITE normalized_alias ON project TYPE string ASSERT $value != NONE AND $value = string::lowercase(string::trim($value)) AND string::len($value) > 0;
DEFINE FIELD OVERWRITE display_name     ON project TYPE option<string>;
DEFINE FIELD OVERWRITE created_at       ON project TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE updated_at       ON project TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_project_alias ON project FIELDS workspace, normalized_alias UNIQUE;

-- Additional explicit aliases — a repository's second remote, or a renamed one that still resolves.
-- One row per alias rather than an array (see the multi-key index note above), unique within the
-- workspace, and never fuzzily merged: Task 8 adds and removes these on purpose.
DEFINE TABLE OVERWRITE project_alias SCHEMAFULL;
DEFINE FIELD OVERWRITE workspace  ON project_alias TYPE record<workspace> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE project    ON project_alias TYPE record<project> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE alias      ON project_alias TYPE string ASSERT $value != NONE AND $value = string::lowercase(string::trim($value)) AND string::len($value) > 0;
DEFINE FIELD OVERWRITE created_at ON project_alias TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_project_alias_lookup ON project_alias FIELDS workspace, alias UNIQUE;
DEFINE INDEX OVERWRITE idx_project_alias_project ON project_alias FIELDS project;

-- ---------------------------------------------------------------------------------------------
-- Credentials — one identity table, `kind` discriminator
-- ---------------------------------------------------------------------------------------------

-- Browser sessions, agent bearer tokens, invitations and recovery links share one table because
-- revocation names one id regardless of kind (`revoke_credential(owner, id)`) and a principal's
-- sealed provenance carries one `CredentialId`. Task 4 owns the crypto; the columns below are the
-- live state `AccessRepo::authorize` re-reads on every call, so a revocation, an expiry or a
-- recovery is reflected without trusting the token the caller already holds.
--
-- `credential.account` is required, so a link is a credential *of the account that granted it*: an
-- invitation belongs to its issuer (the invited account does not exist until the link is claimed), and a
-- recovery link belongs to the account it takes over. Task 4's `create_invitation` documents the
-- consequence — an issuer can see and revoke the invitations it handed out.
DEFINE TABLE OVERWRITE credential SCHEMAFULL;
DEFINE FIELD OVERWRITE account      ON credential TYPE record<account> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE kind         ON credential TYPE string ASSERT $value IN ['browser', 'agent', 'invite', 'recovery'];
-- Named tokens, so an owner can tell which one to revoke. Invitation and recovery links need no name.
DEFINE FIELD OVERWRITE label        ON credential TYPE option<string> ASSERT $value = NONE OR string::len(string::trim($value)) > 0;
-- Digest of the secret, never the secret: bearer tokens, session cookies and one-use links are each
-- shown once and stored only as a hash. Task 4 writes it. The index below is deliberately not
-- UNIQUE: a rotation writes a new row for the same kind, and a NONE digest must not collide.
DEFINE FIELD OVERWRITE secret_digest ON credential TYPE option<string> ASSERT $value = NONE OR string::len($value) >= 16;
-- The `account.auth_epoch` this credential was minted under.
DEFINE FIELD OVERWRITE auth_epoch   ON credential TYPE int DEFAULT 1 ASSERT $value >= 1;
-- `revoked` by its owner or an admin; `consumed` is the atomic one-use claim of an invitation or
-- recovery link. Two booleans rather than one status string because they answer different
-- questions and a consumed link is not a revocation.
DEFINE FIELD OVERWRITE revoked      ON credential TYPE bool DEFAULT false;
DEFINE FIELD OVERWRITE consumed     ON credential TYPE bool DEFAULT false;
-- Two deadlines rather than one, because the plan's browser policy is two clocks: `expires_at` is the
-- rolling idle deadline (pushed out by each authenticated use, and clamped to the cap below), and
-- `absolute_expires_at` is the deadline no amount of use can cross. One column could express either rule
-- and neither of them together: a pure absolute expiry survives use for a month, a pure idle deadline can
-- be kept alive forever by a scripted poller. Both are `option<datetime>` with NO default, so a credential
-- row written before this column existed keeps reading `NONE` and every legacy `UPDATE` of it still
-- succeeds (measured on 3.3.0: a non-optional `DEFAULT` column is what breaks those writes). `NONE` means
-- unconstrained here — which is correct for an agent token, whose lifetime the plan defines as revocation
-- or the account's epoch rather than a date — so every reader must test `= NONE`, never `IS NULL`.
DEFINE FIELD OVERWRITE expires_at   ON credential TYPE option<datetime>;
DEFINE FIELD OVERWRITE absolute_expires_at ON credential TYPE option<datetime>;
-- The CSRF half of a browser session, digested like the session half and for the same reason: the plaintext
-- is shown once and never persisted. A separate column, not a second row, because a CSRF token is not a
-- credential — it authorises nothing on its own and must never resolve through `idx_credential_digest`.
-- The same minimum length as `secret_digest`, which is what a SHA-256 of 32 bytes renders to in base64.
DEFINE FIELD OVERWRITE csrf_digest   ON credential TYPE option<string> ASSERT $value = NONE OR string::len($value) >= 16;
DEFINE FIELD OVERWRITE revoked_at   ON credential TYPE option<datetime>;
DEFINE FIELD OVERWRITE consumed_at  ON credential TYPE option<datetime>;
-- The last authenticated use. Audit, and the only place a session's idleness can be measured from after a
-- restart. Deliberately NOT the heat/stability access clock, which stays on `heat_state` (see AGENTS.md:
-- the access clock and the decay anchor are two columns and must stay two).
DEFINE FIELD OVERWRITE last_used_at ON credential TYPE option<datetime>;
DEFINE FIELD OVERWRITE created_at   ON credential TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_credential_account ON credential FIELDS account, kind;
-- Task 4 authenticates by digest: a token, cookie or one-use link resolves through this index
-- instead of scanning every credential in the store. It covers `secret_digest` only, never `csrf_digest`,
-- which is what keeps a CSRF token from being usable as a session.
DEFINE INDEX OVERWRITE idx_credential_digest ON credential FIELDS secret_digest;

-- ---------------------------------------------------------------------------------------------
-- Ownership-migration state — Task 14 writes it; nothing here nominates an owner
-- ---------------------------------------------------------------------------------------------

DEFINE TABLE OVERWRITE ownership_migration SCHEMAFULL;
-- `pending` is what a store that predates v009 reads as, which is why a later boot fails closed on
-- it instead of serving a half-owned corpus. `paused` is the crash-restart state, `verifying` is
-- the phase that still cannot serve, and only `complete` may.
DEFINE FIELD OVERWRITE state               ON ownership_migration TYPE string DEFAULT 'pending' ASSERT $value IN ['pending', 'running', 'paused', 'verifying', 'complete', 'failed'];
-- Both are required before `state` may be `complete`, which Task 14's own transaction enforces —
-- the schema cannot express "required only in that state", and defaulting them here would be
-- exactly the guessed owner the design forbids.
DEFINE FIELD OVERWRITE nominated_owner     ON ownership_migration TYPE option<record<account>>;
DEFINE FIELD OVERWRITE nominated_workspace ON ownership_migration TYPE option<record<workspace>>;
-- A receipt names a verified snapshot of the source files (Task 14 reuses the offline-copy
-- receipts), not a boolean "backed up" flag.
DEFINE FIELD OVERWRITE backup_receipt      ON ownership_migration TYPE option<string> ASSERT $value = NONE OR string::len(string::trim($value)) > 0;
-- Stable per-phase cursors, so a restart resumes the same run instead of re-nominating an owner.
-- Each is typed as the table it pages, so a cursor that drifted onto another table is a refused
-- write rather than a silent full re-scan.
DEFINE FIELD OVERWRITE fact_cursor         ON ownership_migration TYPE option<record<fact>>;
DEFINE FIELD OVERWRITE session_cursor      ON ownership_migration TYPE option<record<session>>;
DEFINE FIELD OVERWRITE reminder_cursor     ON ownership_migration TYPE option<record<reminder>>;
DEFINE FIELD OVERWRITE rows_migrated       ON ownership_migration TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE report              ON ownership_migration TYPE option<string>;
DEFINE FIELD OVERWRITE started_at          ON ownership_migration TYPE option<datetime>;
DEFINE FIELD OVERWRITE updated_at          ON ownership_migration TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE finished_at         ON ownership_migration TYPE option<datetime>;
DEFINE INDEX OVERWRITE idx_ownership_migration_state ON ownership_migration FIELDS state;

-- ---------------------------------------------------------------------------------------------
-- Private classification proposals — the agent proposes, a human applies
-- ---------------------------------------------------------------------------------------------

-- A row here carries no application authority: Task 11's approval re-checks the fact's revision and
-- moves it through Task 10's journal. The proposal records the revision it was formed against, so a
-- later scope move or edit stales it rather than being overwritten by an old approval.
DEFINE TABLE OVERWRITE classification_proposal SCHEMAFULL;
DEFINE FIELD OVERWRITE workspace  ON classification_proposal TYPE record<workspace> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE account    ON classification_proposal TYPE record<account> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE fact       ON classification_proposal TYPE record<fact> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE destination_kind ON classification_proposal TYPE string ASSERT $value IN ['personal', 'project', 'unassigned']
    AND ($value != 'project' OR $this.destination_project != NONE);
-- A `project` destination must name a project, and no other destination may — through `$this`, the
-- only sibling-reference spelling this engine evaluates (see the note above). Whether that project
-- belongs to *this* workspace is a read, not a type check, and is refused at the resolver
-- (`proposals_are_workspace_private_and_typed` covers both halves at the authorization seam).
DEFINE FIELD OVERWRITE destination_project ON classification_proposal TYPE option<record<project>> ASSERT $value = NONE OR $this.destination_kind = 'project';
DEFINE FIELD OVERWRITE source_revision     ON classification_proposal TYPE option<int> ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE revision            ON classification_proposal TYPE int DEFAULT 0 ASSERT $value >= 0;
-- `approved` is a human decision recorded by Task 11; nothing in this ladder is automatic.
DEFINE FIELD OVERWRITE state               ON classification_proposal TYPE string DEFAULT 'pending' ASSERT $value IN ['pending', 'approved', 'dismissed', 'stale'];
DEFINE FIELD OVERWRITE reason              ON classification_proposal TYPE option<string>;
DEFINE FIELD OVERWRITE created_at          ON classification_proposal TYPE datetime DEFAULT time::now();
DEFINE FIELD OVERWRITE decided_at          ON classification_proposal TYPE option<datetime>;
DEFINE FIELD OVERWRITE decided_by          ON classification_proposal TYPE option<record<account>>;
DEFINE INDEX OVERWRITE idx_classification_proposal_state ON classification_proposal FIELDS workspace, state;
DEFINE INDEX OVERWRITE idx_classification_proposal_fact ON classification_proposal FIELDS fact;

-- ---------------------------------------------------------------------------------------------
-- The shared private semantic journal (Task 10 is its writer; Task 20/21 read it back)
-- ---------------------------------------------------------------------------------------------

DEFINE TABLE OVERWRITE semantic_operation SCHEMAFULL;
DEFINE FIELD OVERWRITE workspace ON semantic_operation TYPE record<workspace> ASSERT $value != NONE;
-- The account whose authority the write happened under, and the credential that actually presented
-- itself. `initiator` is NONE only for startup/system and trusted-local authority — that is the
-- audit answer to "browser, agent token, or the operator?" without a second table.
DEFINE FIELD OVERWRITE actor     ON semantic_operation TYPE record<account> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE initiator ON semantic_operation TYPE option<record<credential>>;
-- Closed operation kinds (Task 10's `operation_kind`): ordinary edits and deletes, scope moves,
-- maintenance semantic changes, refinement application and revert, plus operator enrollment. A new
-- kind is a new migration, not a new string.
DEFINE FIELD OVERWRITE kind ON semantic_operation TYPE string ASSERT $value IN ['store', 'import', 'update', 'delete', 'move', 'cluster_change', 'collapse', 'appraise', 'reminder_map', 'quality', 'revert', 'bootstrap_owner'];
DEFINE FIELD OVERWRITE operation_key ON semantic_operation TYPE string ASSERT $value != NONE AND string::len(string::trim($value)) > 0;
-- The fingerprint of what was requested. Reusing a key for a different request must conflict rather
-- than return somebody's earlier success; the UNIQUE index makes a second row for
-- (workspace, key) impossible, so a writer must compare the fingerprint before it could replay one.
DEFINE FIELD OVERWRITE request_fingerprint ON semantic_operation TYPE string ASSERT $value != NONE AND string::len($value) >= 16;
-- Exact expected and actual effect sets, and the partitions/records the operation touched. Scope
-- kinds are the three shapes; projects are named by handle, never by text, so two workspaces'
-- `alexandria` rows cannot be confused by a string.
DEFINE FIELD OVERWRITE expected_effects ON semantic_operation TYPE array<string> ASSERT array::len($value) > 0 AND $value.all(|$v| string::len($v) > 0);
DEFINE FIELD OVERWRITE actual_effects   ON semantic_operation TYPE option<array<string>>;
DEFINE FIELD OVERWRITE affected_scope_kinds ON semantic_operation TYPE array<string> DEFAULT [] ASSERT $value.all(|$v| $v INSIDE ['personal', 'project', 'unassigned']);
DEFINE FIELD OVERWRITE affected_projects    ON semantic_operation TYPE array<record<project>> DEFAULT [];
DEFINE FIELD OVERWRITE affected_ids         ON semantic_operation TYPE array<record> DEFAULT [];
-- Pre/postimage references and their digests.
DEFINE FIELD OVERWRITE preimages      ON semantic_operation TYPE array<record<semantic_snapshot>> DEFAULT [];
DEFINE FIELD OVERWRITE postimages     ON semantic_operation TYPE array<record<semantic_snapshot>> DEFAULT [];
DEFINE FIELD OVERWRITE preimage_hash  ON semantic_operation TYPE option<string>;
DEFINE FIELD OVERWRITE postimage_hash ON semantic_operation TYPE option<string>;
DEFINE FIELD OVERWRITE committed_at   ON semantic_operation TYPE option<datetime>;
-- A revert names the operation it reverted and the reverted operation names its revert. The table is
-- referenced by its own name; `record<self>` is not accepted on this engine.
DEFINE FIELD OVERWRITE reverts        ON semantic_operation TYPE option<record<semantic_operation>>;
DEFINE FIELD OVERWRITE reverted_by    ON semantic_operation TYPE option<record<semantic_operation>>;
DEFINE FIELD OVERWRITE created_at     ON semantic_operation TYPE datetime DEFAULT time::now();
-- Never a global client-chosen key: unique per workspace, so two owners may both use `retry-1` and
-- neither resolves the other's operation.
DEFINE INDEX OVERWRITE idx_semantic_operation_key ON semantic_operation FIELDS workspace, operation_key UNIQUE;

DEFINE TABLE OVERWRITE semantic_snapshot SCHEMAFULL;
DEFINE FIELD OVERWRITE workspace ON semantic_snapshot TYPE record<workspace> ASSERT $value != NONE;
DEFINE FIELD OVERWRITE operation ON semantic_snapshot TYPE record<semantic_operation> ASSERT $value != NONE;
-- The variant discriminator, declared ahead of every payload column: on this engine a payload
-- column's `$this.kind` assertion only sees the discriminator when it is defined first (measured —
-- declared last, a `fact` snapshot accepted a `reminder_message`), so the rule that makes
-- "typed variants" real sits here, and each payload column repeats it from the other side below.
--
-- Three properties, all engine-enforced:
--   1. the kind is one of the four (an unknown variant is a refused write);
--   2. the variant arrives with its own handle and its core payload, so there is something to
--      restore from — a `fact` snapshot with no fact and no content is decoration;
--   3. the subject is a record of the variant's own table. `meta::tb($this.subject)` is the only
--      way to compare a record id's table inside an assertion, and it evaluates here (measured).
DEFINE FIELD OVERWRITE kind ON semantic_snapshot TYPE string ASSERT $value IN ['fact', 'edge', 'cluster', 'reminder']
    AND ($value != 'fact' OR ($this.fact != NONE AND $this.fact_content != NONE))
    AND ($value != 'edge' OR $this.edge != NONE)
    AND ($value != 'cluster' OR $this.cluster != NONE)
    AND ($value != 'reminder' OR $this.reminder != NONE)
    AND (
        ($value = 'fact' AND meta::tb($this.subject) = 'fact')
        OR ($value = 'edge' AND meta::tb($this.subject) = 'memory_edge')
        OR ($value = 'cluster' AND meta::tb($this.subject) = 'cluster')
        OR ($value = 'reminder' AND meta::tb($this.subject) = 'reminder')
    );
DEFINE FIELD OVERWRITE subject   ON semantic_snapshot TYPE record ASSERT $value != NONE;

-- fact variant: the semantic state and the embedding identity it was written under. Deliberately no
-- `heat`, `stability`, `access_count`, `last_touched` or `last_accessed_at` column anywhere in this
-- table — SCHEMAFULL rejects an undefined column outright (measured: "Found field 'heat', but no
-- such field exists for table"), so a snapshot cannot become a container for restoring an access
-- clock over a later real access. "Preserve later legitimate heat/access history" is therefore a
-- property of the table, not of the writer's restraint.
DEFINE FIELD OVERWRITE fact                     ON semantic_snapshot TYPE option<record<fact>> ASSERT $value = NONE OR $this.kind = 'fact';
DEFINE FIELD OVERWRITE fact_content             ON semantic_snapshot TYPE option<string> ASSERT $value = NONE OR $this.kind = 'fact';
DEFINE FIELD OVERWRITE fact_tags                ON semantic_snapshot TYPE option<array<string>> ASSERT $this.kind = 'fact';
DEFINE FIELD OVERWRITE fact_confidence          ON semantic_snapshot TYPE option<float> ASSERT $this.kind = 'fact';
DEFINE FIELD OVERWRITE fact_scope               ON semantic_snapshot TYPE option<string> ASSERT $value = NONE OR $value INSIDE ['personal', 'project', 'unassigned'];
DEFINE FIELD OVERWRITE fact_project             ON semantic_snapshot TYPE option<record<project>>;
DEFINE FIELD OVERWRITE fact_revision            ON semantic_snapshot TYPE option<int> ASSERT $value = NONE OR $value >= 0;
-- Visibility is semantic — it is what a move or a quarantine decision changed — so it is recorded;
-- the access clocks above are not, and are what a restore must never write back.
DEFINE FIELD OVERWRITE fact_deleted             ON semantic_snapshot TYPE option<bool> ASSERT $this.kind = 'fact';
DEFINE FIELD OVERWRITE fact_quarantined_at      ON semantic_snapshot TYPE option<datetime> ASSERT $this.kind = 'fact';
-- Embedding identity, not just the vector: a postimage compared against a vector from a different
-- space is meaningless, and this is what makes the comparison honest.
DEFINE FIELD OVERWRITE fact_embedding           ON semantic_snapshot TYPE option<array<float>> ASSERT $this.kind = 'fact';
DEFINE FIELD OVERWRITE fact_embedding_model     ON semantic_snapshot TYPE option<string> ASSERT $this.kind = 'fact';
DEFINE FIELD OVERWRITE fact_embedding_dimensions ON semantic_snapshot TYPE option<int> ASSERT $value = NONE OR $value > 0;
-- "A fact revision alone cannot certify an unchanged graph": the guarded edge ids ride with the
-- fact's preimage so a rollback refuses when the graph moved.
DEFINE FIELD OVERWRITE fact_relations           ON semantic_snapshot TYPE option<array<record<memory_edge>>> ASSERT $value = NONE OR $this.kind = 'fact';

-- edge variant: exact endpoints and the edge's own state.
DEFINE FIELD OVERWRITE edge          ON semantic_snapshot TYPE option<record<memory_edge>> ASSERT $value = NONE OR $this.kind = 'edge';
DEFINE FIELD OVERWRITE edge_in       ON semantic_snapshot TYPE option<record> ASSERT $this.kind = 'edge';
DEFINE FIELD OVERWRITE edge_out      ON semantic_snapshot TYPE option<record> ASSERT $this.kind = 'edge';
DEFINE FIELD OVERWRITE edge_type     ON semantic_snapshot TYPE option<string> ASSERT $this.kind = 'edge';
DEFINE FIELD OVERWRITE edge_strength ON semantic_snapshot TYPE option<float> ASSERT $this.kind = 'edge';

-- cluster variant: membership, centroid and all three guards, so an unrelated derived-bookkeeping
-- move is distinguishable from a reviewed-membership conflict instead of being resolved by
-- overwriting one with the other.
DEFINE FIELD OVERWRITE cluster                           ON semantic_snapshot TYPE option<record<cluster>> ASSERT $value = NONE OR $this.kind = 'cluster';
DEFINE FIELD OVERWRITE cluster_label                     ON semantic_snapshot TYPE option<string> ASSERT $this.kind = 'cluster';
DEFINE FIELD OVERWRITE cluster_centroid                  ON semantic_snapshot TYPE option<array<float>> ASSERT $this.kind = 'cluster';
DEFINE FIELD OVERWRITE cluster_depth                     ON semantic_snapshot TYPE option<int> ASSERT $this.kind = 'cluster';
DEFINE FIELD OVERWRITE cluster_members                   ON semantic_snapshot TYPE option<array<record<fact>>> ASSERT $this.kind = 'cluster';
DEFINE FIELD OVERWRITE cluster_membership_revision       ON semantic_snapshot TYPE option<int> ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE cluster_member_embedding_revision ON semantic_snapshot TYPE option<int> ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE cluster_centroid_revision         ON semantic_snapshot TYPE option<int> ASSERT $value = NONE OR $value >= 0;

-- Reminder payload columns take the form `$value = NONE OR ($this.kind = 'reminder' AND …)`.
-- Parenthesising matters: `A AND $value = NONE OR B` parses as `(A AND $value = NONE) OR B`, so the
-- bare-and form silently let a `fact` snapshot carry `reminder_semantic_revision` or
-- `reminder_schedule_kind`, and `snapshots_are_typed_…` refuses both shapes today.
--
-- reminder variant: the mapping decision and the schedule it belongs to. Deliberately no `status`,
-- `next_due_at`, `delivered_count`, `last_delivered_at` or `cancelled_at` column — a snapshot that
-- could carry the delivery clock is a way to rewind deliveries that happened after it, which is the
-- failure Task 14's release path must not be able to cause.
DEFINE FIELD OVERWRITE reminder                       ON semantic_snapshot TYPE option<record<reminder>> ASSERT $value = NONE OR $this.kind = 'reminder';
DEFINE FIELD OVERWRITE reminder_account               ON semantic_snapshot TYPE option<record<account>> ASSERT $this.kind = 'reminder';
DEFINE FIELD OVERWRITE reminder_message               ON semantic_snapshot TYPE option<string> ASSERT $this.kind = 'reminder';
DEFINE FIELD OVERWRITE reminder_target_project       ON semantic_snapshot TYPE option<string> ASSERT $this.kind = 'reminder';
DEFINE FIELD OVERWRITE reminder_legacy_target_project ON semantic_snapshot TYPE option<string> ASSERT $this.kind = 'reminder';
DEFINE FIELD OVERWRITE reminder_project               ON semantic_snapshot TYPE option<record<project>> ASSERT $this.kind = 'reminder';
DEFINE FIELD OVERWRITE reminder_migration_hold       ON semantic_snapshot TYPE option<bool> ASSERT $this.kind = 'reminder';
DEFINE FIELD OVERWRITE reminder_semantic_revision    ON semantic_snapshot TYPE option<int> ASSERT $value = NONE OR ($this.kind = 'reminder' AND $value >= 0);
DEFINE FIELD OVERWRITE reminder_schedule_kind        ON semantic_snapshot TYPE option<string> ASSERT $value = NONE OR ($this.kind = 'reminder' AND $value INSIDE ['once', 'pattern', 'cron']);
DEFINE FIELD OVERWRITE created_at                    ON semantic_snapshot TYPE datetime DEFAULT time::now();

DEFINE INDEX OVERWRITE idx_semantic_snapshot_operation ON semantic_snapshot FIELDS operation;
DEFINE INDEX OVERWRITE idx_semantic_snapshot_subject ON semantic_snapshot FIELDS workspace, subject;

-- ---------------------------------------------------------------------------------------------
-- Existing owned resources: canonical ownership, scope and semantic revisions
-- ---------------------------------------------------------------------------------------------

-- A memory belongs to one workspace and exactly one scope. All four columns are `option<>` with no
-- default: an existing row reads NONE, the scoped paths refuse it, and Task 14's verified backfill
-- later fills it. `scope` accepts the three shapes or NONE — never a fourth, and never "personal"
-- because nothing else was said.
DEFINE FIELD OVERWRITE account  ON fact TYPE option<record<account>>;
DEFINE FIELD OVERWRITE workspace ON fact TYPE option<record<workspace>>;
DEFINE FIELD OVERWRITE scope    ON fact TYPE option<string> ASSERT $value = NONE OR $value IN ['personal', 'project', 'unassigned'];
DEFINE FIELD OVERWRITE project  ON fact TYPE option<record<project>>;
-- The compare-and-swap target for content/tags/confidence/scope/visibility writes. Absent on a row
-- that predates v009, which readers treat as "not yet guarded" — a writer not yet converted to the
-- journal is covered by Task 10's transactional preimage comparison, not by trusting a counter it
-- never increments.
DEFINE FIELD OVERWRITE semantic_revision ON fact TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;
DEFINE INDEX OVERWRITE idx_fact_partition ON fact FIELDS workspace, scope;

-- Sessions gain the canonical account. `session_external_id_idx` stays exactly as v005 defined it:
-- re-keying the global handle index to `(account, external_id)` has to land in the same checkpoint
-- as the readers and the collision-recovery write (Task 6 / v010), because a store with a qualified
-- index and unqualified readers resolves — and then writes — another account's session. No
-- account-qualified session index is defined here either, so `session_external_id_idx` stays the
-- only handle index on the table until Task 6.
DEFINE FIELD OVERWRITE account  ON session TYPE option<record<account>>;
DEFINE FIELD OVERWRITE workspace ON session TYPE option<record<workspace>>;
DEFINE FIELD OVERWRITE semantic_revision ON session TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;

-- Raw source documents are owner-private content; the ownership path for a chunk is its
-- `extracted_from` edge, so `raw` carries the columns and the relation tables carry none.
DEFINE FIELD OVERWRITE account  ON raw TYPE option<record<account>>;
DEFINE FIELD OVERWRITE workspace ON raw TYPE option<record<workspace>>;
DEFINE FIELD OVERWRITE semantic_revision ON raw TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;

-- Provenance inherits the ownership of the memory it describes through `has_provenance`.
-- `account`/`workspace` are the validated denormalization; legacy *unlinked* rows receive Task 14's
-- nominated owner rather than a guess inferred from `agent_id`, `model` or a legacy session id.
DEFINE FIELD OVERWRITE account  ON provenance TYPE option<record<account>>;
DEFINE FIELD OVERWRITE workspace ON provenance TYPE option<record<workspace>>;

-- Clusters are workspace-scoped derived state: two accounts with byte-identical texts are not one
-- cluster, and an existing cluster is not simply relabelled personal (Task 14 rebuilds memberships
-- as classifications are approved). The three guards are the CAS targets every membership-edge
-- writer, member-embedding writer and centroid writer must increment in the same transaction as its
-- own write — that is what makes an unrelated centroid move a retryable bookkeeping conflict while
-- a reviewed-membership change is a semantic `Stale`, instead of a silent overwrite.
DEFINE FIELD OVERWRITE account  ON cluster TYPE option<record<account>>;
DEFINE FIELD OVERWRITE workspace ON cluster TYPE option<record<workspace>>;
DEFINE FIELD OVERWRITE membership_revision       ON cluster TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE member_embedding_revision ON cluster TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE centroid_revision         ON cluster TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;

-- Reminders: private to one account, with the hold that keeps an unmapped legacy target out of
-- delivery, escalation and the due-summary piggyback. `target_project` (v007) is untouched —
-- Task 14 copies its text into `legacy_target_project` verbatim only after verifying a mapping, and
-- arbitrary legacy names are never cast into `record<project>` ids.
DEFINE FIELD OVERWRITE account   ON reminder TYPE option<record<account>>;
DEFINE FIELD OVERWRITE semantic_revision ON reminder TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE project   ON reminder TYPE option<record<project>>;
DEFINE FIELD OVERWRITE migration_hold ON reminder TYPE option<bool> DEFAULT false;
DEFINE FIELD OVERWRITE legacy_target_project ON reminder TYPE option<string>;

-- Background summaries link to the journal operation they describe, so a dreaming pass that made
-- semantic writes is traceable to its pre/postimages. Optional, because cluster/collapse/demote rows
-- written before this file have no operation to name — and this is a link, not a new public or
-- global memory log.
DEFINE FIELD OVERWRITE operation ON maintenance_log TYPE option<record<semantic_operation>>;
DEFINE INDEX OVERWRITE idx_maintenance_log_operation ON maintenance_log FIELDS operation;

-- ---------------------------------------------------------------------------------------------
-- System maintenance progress — coordination metadata, not corpus and not an audit row
-- ---------------------------------------------------------------------------------------------

-- One versioned record per row-capped job, so a partial pass resumes where it stopped instead of
-- always restarting at the first partition, and one job's spent allowance cannot be re-armed by a
-- restart. `Sweep` has a progress record but still writes no `maintenance_log` row: the audit-job
-- namespace excludes it on purpose (`models::maintenance::job`), and this table is not that
-- namespace. It is system coordination data — excluded from Task 14's ownership backfill and from
-- every owner read path, and not reachable through a browser or an MCP parameter.
DEFINE TABLE OVERWRITE maintenance_progress SCHEMAFULL;
DEFINE FIELD OVERWRITE job ON maintenance_progress TYPE string ASSERT $value IN ['sweep', 'collapse', 'appraise'];
-- The CAS revision Task 12's save guard increments. Absent means "no progress written yet", which
-- is what a first run loads as version zero with empty cursors rather than as a stale ledger.
DEFINE FIELD OVERWRITE version ON maintenance_progress TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE last_workspace ON maintenance_progress TYPE option<record<workspace>>;
-- Per-workspace scope cursors. `workspace`, `scope` and `project` are typed by the engine: a cursor
-- whose workspace names the wrong table, an unknown scope, a missing workspace or a project of the
-- wrong shape is a refused write rather than something a later pass has to distrust. `row_cursor`
-- below is deliberately *generic* (`option<record>`, any table): the three capped jobs do not page one
-- table — `sweep`/`appraise` walk selected heat rows, `collapse` works on facts — so a per-job table
-- restriction is Task 12's validation once those consumers exist, not a claim this file can make.
-- (Nested *unknown* keys are not rejected by SCHEMAFULL on this engine — measured — which is why the
-- snapshot payloads above are flat columns and these cursors are exactly-named fields.)
DEFINE FIELD OVERWRITE cursors ON maintenance_progress TYPE array<object> DEFAULT [];
DEFINE FIELD OVERWRITE cursors[*].workspace ON maintenance_progress TYPE record<workspace>;
DEFINE FIELD OVERWRITE cursors[*].scope ON maintenance_progress TYPE string ASSERT $value IN ['personal', 'project', 'unassigned'];
DEFINE FIELD OVERWRITE cursors[*].project ON maintenance_progress TYPE option<record<project>>;
DEFINE FIELD OVERWRITE cursors[*].row_cursor ON maintenance_progress TYPE option<record>;
DEFINE FIELD OVERWRITE cursors[*].units_done ON maintenance_progress TYPE option<int> DEFAULT 0 ASSERT $value = NONE OR $value >= 0;
DEFINE FIELD OVERWRITE cursors[*].exhausted ON maintenance_progress TYPE option<bool> DEFAULT false;
DEFINE FIELD OVERWRITE updated_at ON maintenance_progress TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_maintenance_progress_job ON maintenance_progress FIELDS job UNIQUE;
