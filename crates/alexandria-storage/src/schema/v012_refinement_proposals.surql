-- v012: immutable proposal/evidence payloads and canonical evidence identity.
-- v011 introduced policy/admission tables before proposal generation consumed them.
DEFINE FIELD OVERWRITE evidence_fingerprint ON refinement_proposal TYPE string ASSERT string::len($value) = 64;
DEFINE FIELD OVERWRITE source_fingerprint   ON refinement_proposal TYPE string ASSERT string::len($value) = 64;
DEFINE FIELD OVERWRITE proposal_payload     ON refinement_proposal TYPE string;
DEFINE FIELD OVERWRITE payload              ON refinement_evidence TYPE string;
DEFINE INDEX OVERWRITE idx_refinement_proposal_evidence ON refinement_proposal FIELDS account, evidence_fingerprint, source_fingerprint, revision;
