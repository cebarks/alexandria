-- v011: private refinement state. Configuration is disabled by an absent inference_policy_id;
-- migrating a store creates neither a configuration nor an owner grant. The semantic_operation
-- and semantic_snapshot tables from v009 remain the sole journal for corpus pre/postimages.

DEFINE TABLE OVERWRITE refinement_policy SCHEMAFULL;
DEFINE FIELD OVERWRITE revision            ON refinement_policy TYPE int ASSERT $value >= 0;
DEFINE FIELD OVERWRITE destination_revision ON refinement_policy TYPE int ASSERT $value >= 0;
DEFINE FIELD OVERWRITE destination_set     ON refinement_policy TYPE array<object> ASSERT array::len($value) <= 2;
DEFINE FIELD OVERWRITE destination_set.*.protocol ON refinement_policy TYPE string ASSERT $value IN ['open_ai_chat_completions', 'anthropic_messages'];
DEFINE FIELD OVERWRITE destination_set.*.scheme ON refinement_policy TYPE string ASSERT $value IN ['http', 'https'];
DEFINE FIELD OVERWRITE destination_set.*.host ON refinement_policy TYPE string ASSERT $value != '';
DEFINE FIELD OVERWRITE destination_set.*.effective_port ON refinement_policy TYPE int ASSERT $value > 0 AND $value <= 65535;
DEFINE FIELD OVERWRITE destination_set.*.api_prefix ON refinement_policy TYPE string;
DEFINE FIELD OVERWRITE inference_policy_id ON refinement_policy TYPE option<string> ASSERT $value = NONE OR string::len($value) = 64;
DEFINE FIELD OVERWRITE allow_automatic     ON refinement_policy TYPE bool;
DEFINE FIELD OVERWRITE write_fence         ON refinement_policy TYPE bool;

DEFINE TABLE OVERWRITE refinement_consent SCHEMAFULL;
DEFINE FIELD OVERWRITE account              ON refinement_consent TYPE record<account>;
DEFINE FIELD OVERWRITE revision             ON refinement_consent TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE destination_revision ON refinement_consent TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE destination_set      ON refinement_consent TYPE array<object> ASSERT array::len($value) > 0 AND array::len($value) <= 2;
DEFINE FIELD OVERWRITE destination_set.*.protocol ON refinement_consent TYPE string ASSERT $value IN ['open_ai_chat_completions', 'anthropic_messages'];
DEFINE FIELD OVERWRITE destination_set.*.scheme ON refinement_consent TYPE string ASSERT $value IN ['http', 'https'];
DEFINE FIELD OVERWRITE destination_set.*.host ON refinement_consent TYPE string ASSERT $value != '';
DEFINE FIELD OVERWRITE destination_set.*.effective_port ON refinement_consent TYPE int ASSERT $value > 0 AND $value <= 65535;
DEFINE FIELD OVERWRITE destination_set.*.api_prefix ON refinement_consent TYPE string;
DEFINE FIELD OVERWRITE granted_at ON refinement_consent TYPE datetime;
DEFINE FIELD OVERWRITE revoked_at ON refinement_consent TYPE option<datetime>;
DEFINE INDEX OVERWRITE idx_refinement_consent_account ON refinement_consent FIELDS account UNIQUE;

DEFINE TABLE OVERWRITE refinement_approval_mode SCHEMAFULL;
DEFINE FIELD OVERWRITE account  ON refinement_approval_mode TYPE record<account>;
DEFINE FIELD OVERWRITE revision ON refinement_approval_mode TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE mode     ON refinement_approval_mode TYPE string ASSERT $value IN ['human', 'secondary_llm'];
DEFINE INDEX OVERWRITE idx_refinement_approval_account ON refinement_approval_mode FIELDS account UNIQUE;

-- Evidence describes the text/sources seen by inference. It is not a mutation snapshot.
DEFINE TABLE OVERWRITE refinement_evidence SCHEMAFULL;
DEFINE FIELD OVERWRITE account             ON refinement_evidence TYPE record<account>;
DEFINE FIELD OVERWRITE workspace           ON refinement_evidence TYPE record<workspace>;
DEFINE FIELD OVERWRITE project             ON refinement_evidence TYPE option<record<project>>;
DEFINE FIELD OVERWRITE scope               ON refinement_evidence TYPE string ASSERT $value IN ['personal', 'project', 'unassigned'];
DEFINE FIELD OVERWRITE focal               ON refinement_evidence TYPE record<fact>;
DEFINE FIELD OVERWRITE related             ON refinement_evidence TYPE array<record<fact>> DEFAULT [];
DEFINE FIELD OVERWRITE inference_policy_id ON refinement_evidence TYPE string ASSERT string::len($value) = 64;
DEFINE FIELD OVERWRITE created_at          ON refinement_evidence TYPE datetime DEFAULT time::now();

-- Each row binds one immutable proposal revision to its original authority. Review flags
-- are separate from the lifecycle; there is no approved-but-unapplied authority state.
DEFINE TABLE OVERWRITE refinement_proposal SCHEMAFULL;
DEFINE FIELD OVERWRITE account              ON refinement_proposal TYPE record<account>;
DEFINE FIELD OVERWRITE workspace            ON refinement_proposal TYPE record<workspace>;
DEFINE FIELD OVERWRITE project              ON refinement_proposal TYPE option<record<project>>;
DEFINE FIELD OVERWRITE scope                ON refinement_proposal TYPE string ASSERT $value IN ['personal', 'project', 'unassigned'];
DEFINE FIELD OVERWRITE proposal_key         ON refinement_proposal TYPE string;
DEFINE FIELD OVERWRITE revision             ON refinement_proposal TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE state                ON refinement_proposal TYPE string ASSERT $value IN ['pending', 'applied', 'dismissed', 'stale', 'reverted'];
DEFINE FIELD OVERWRITE needs_human          ON refinement_proposal TYPE bool DEFAULT false;
DEFINE FIELD OVERWRITE deferred             ON refinement_proposal TYPE bool DEFAULT false;
DEFINE FIELD OVERWRITE evidence             ON refinement_proposal TYPE record<refinement_evidence>;
DEFINE FIELD OVERWRITE origin_policy       ON refinement_proposal TYPE object;
DEFINE FIELD OVERWRITE origin_policy.account ON refinement_proposal TYPE record<account>;
DEFINE FIELD OVERWRITE origin_policy.consent_revision ON refinement_proposal TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE origin_policy.owner_mode_revision ON refinement_proposal TYPE int ASSERT $value >= 0;
DEFINE FIELD OVERWRITE origin_policy.inference_policy_id ON refinement_proposal TYPE string ASSERT string::len($value) = 64;
DEFINE FIELD OVERWRITE origin_policy.configured_revision ON refinement_proposal TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE origin_policy.mode  ON refinement_proposal TYPE string ASSERT $value IN ['human', 'secondary_llm'];
DEFINE FIELD OVERWRITE operation           ON refinement_proposal TYPE option<record<semantic_operation>>;
DEFINE FIELD OVERWRITE created_at           ON refinement_proposal TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_refinement_proposal_revision ON refinement_proposal FIELDS account, proposal_key, revision UNIQUE;
DEFINE INDEX OVERWRITE idx_refinement_proposal_queue    ON refinement_proposal FIELDS account, state, created_at;

DEFINE TABLE OVERWRITE refinement_attempt SCHEMAFULL;
DEFINE FIELD OVERWRITE account           ON refinement_attempt TYPE record<account>;
DEFINE FIELD OVERWRITE workspace         ON refinement_attempt TYPE record<workspace>;
DEFINE FIELD OVERWRITE project           ON refinement_attempt TYPE option<record<project>>;
DEFINE FIELD OVERWRITE scope             ON refinement_attempt TYPE string ASSERT $value IN ['personal', 'project', 'unassigned'];
DEFINE FIELD OVERWRITE proposal          ON refinement_attempt TYPE option<record<refinement_proposal>>;
DEFINE FIELD OVERWRITE proposal_revision ON refinement_attempt TYPE option<int> ASSERT $value = NONE OR $value > 0;
DEFINE FIELD OVERWRITE state             ON refinement_attempt TYPE string ASSERT $value IN ['queued', 'running', 'completed', 'cancelled', 'failed'];
DEFINE FIELD OVERWRITE authority         ON refinement_attempt TYPE object;
DEFINE FIELD OVERWRITE authority.account ON refinement_attempt TYPE record<account>;
DEFINE FIELD OVERWRITE authority.consent_revision ON refinement_attempt TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE authority.owner_mode_revision ON refinement_attempt TYPE int ASSERT $value >= 0;
DEFINE FIELD OVERWRITE authority.inference_policy_id ON refinement_attempt TYPE string ASSERT string::len($value) = 64;
DEFINE FIELD OVERWRITE authority.configured_revision ON refinement_attempt TYPE int ASSERT $value > 0;
DEFINE FIELD OVERWRITE authority.mode   ON refinement_attempt TYPE string ASSERT $value IN ['human', 'secondary_llm'];
DEFINE FIELD OVERWRITE lease_until       ON refinement_attempt TYPE option<datetime>;
DEFINE FIELD OVERWRITE created_at         ON refinement_attempt TYPE datetime DEFAULT time::now();
DEFINE INDEX OVERWRITE idx_refinement_attempt_queue ON refinement_attempt FIELDS state, lease_until, created_at;
DEFINE INDEX OVERWRITE idx_refinement_attempt_owner ON refinement_attempt FIELDS account, created_at;

-- The later worker reserves counts before spending a call. Failed calls can still have cost.
DEFINE TABLE OVERWRITE refinement_budget SCHEMAFULL;
DEFINE FIELD OVERWRITE bucket        ON refinement_budget TYPE string ASSERT $value IN ['pass', 'account', 'global'];
DEFINE FIELD OVERWRITE account       ON refinement_budget TYPE option<record<account>>;
DEFINE FIELD OVERWRITE window_start  ON refinement_budget TYPE datetime;
DEFINE FIELD OVERWRITE window_end    ON refinement_budget TYPE datetime;
DEFINE FIELD OVERWRITE attempted_calls ON refinement_budget TYPE int DEFAULT 0 ASSERT $value >= 0;
DEFINE FIELD OVERWRITE request_bytes   ON refinement_budget TYPE int DEFAULT 0 ASSERT $value >= 0;
DEFINE FIELD OVERWRITE response_bytes  ON refinement_budget TYPE int DEFAULT 0 ASSERT $value >= 0;
DEFINE INDEX OVERWRITE idx_refinement_budget_expiry ON refinement_budget FIELDS window_end;
