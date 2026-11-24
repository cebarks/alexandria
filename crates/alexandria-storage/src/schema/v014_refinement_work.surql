-- v014: durable refinement work-queue fields. v011's refinement_attempt row was proposal-shaped;
-- the fair worker also needs the work class, the owner's idempotency key and the selection the
-- request named, so a restart, a double submit or a crash recovery resolves to the same work.
-- Enqueueing refinement work writes no maintenance_log row: the audit-job namespace stays at
-- its four entries (cluster/merge/collapse/appraise) and sweep stays deliberately absent.
DEFINE FIELD OVERWRITE kind            ON refinement_attempt TYPE string ASSERT $value IN ['assess', 'opinion', 'regenerate', 'assess_unassigned'];
DEFINE FIELD OVERWRITE idempotency_key ON refinement_attempt TYPE string ASSERT string::len($value) > 0 AND string::len($value) <= 256;
DEFINE FIELD OVERWRITE source_revision ON refinement_attempt TYPE option<int> ASSERT $value = NONE OR $value > 0;
DEFINE FIELD OVERWRITE selection        ON refinement_attempt TYPE object;
DEFINE FIELD OVERWRITE selection.kind   ON refinement_attempt TYPE string ASSERT $value IN ['scheduled', 'evidence'];
DEFINE FIELD OVERWRITE selection.focal  ON refinement_attempt TYPE option<record<fact>>;
DEFINE FIELD OVERWRITE selection.related ON refinement_attempt TYPE array<record<fact>> DEFAULT [];
DEFINE INDEX OVERWRITE idx_refinement_attempt_idempotency ON refinement_attempt FIELDS account, idempotency_key UNIQUE;
DEFINE INDEX OVERWRITE idx_refinement_attempt_proposal    ON refinement_attempt FIELDS account, proposal, state;
