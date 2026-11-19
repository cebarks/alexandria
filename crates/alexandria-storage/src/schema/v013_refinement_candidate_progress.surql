-- Only explicit candidate acknowledgements write this checkpoint. No jobs or attempts live here.
DEFINE TABLE OVERWRITE refinement_candidate_progress SCHEMAFULL;
DEFINE FIELD OVERWRITE account ON refinement_candidate_progress TYPE record<account>;
DEFINE FIELD OVERWRITE workspace ON refinement_candidate_progress TYPE record<workspace>;
DEFINE FIELD OVERWRITE scope ON refinement_candidate_progress TYPE string ASSERT $value IN ['personal', 'project'];
DEFINE FIELD OVERWRITE project ON refinement_candidate_progress TYPE option<record<project>>;
DEFINE FIELD OVERWRITE after_fact ON refinement_candidate_progress TYPE option<record<fact>>;
DEFINE FIELD OVERWRITE revision ON refinement_candidate_progress TYPE int ASSERT $value > 0;
