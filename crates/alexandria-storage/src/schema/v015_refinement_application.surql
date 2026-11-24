-- Generated memberships attribute a result to its source fact, not a new user assertion.
DEFINE FIELD OVERWRITE generated_from ON contains_session_memory TYPE option<record<fact>>;

-- The ordinary journal projection remains unchanged; quality rollback needs the complete
-- incident graph, including structural relation ids and the generated attribution.
DEFINE FIELD OVERWRITE quality_graph ON semantic_snapshot TYPE option<array<object>>;
DEFINE FIELD OVERWRITE quality_graph.*.edge ON semantic_snapshot TYPE record;
DEFINE FIELD OVERWRITE quality_graph.*.kind ON semantic_snapshot TYPE string;
DEFINE FIELD OVERWRITE quality_graph.*.source ON semantic_snapshot TYPE record;
DEFINE FIELD OVERWRITE quality_graph.*.target ON semantic_snapshot TYPE record;
DEFINE FIELD OVERWRITE quality_graph.*.strength ON semantic_snapshot TYPE float;
DEFINE FIELD OVERWRITE quality_graph.*.generated_from ON semantic_snapshot TYPE option<record<fact>>;

-- Application failures are terminal attempts, never runnable queue items or approval tokens.
DEFINE FIELD OVERWRITE kind ON refinement_attempt TYPE string ASSERT $value IN ['assess', 'opinion', 'regenerate', 'assess_unassigned', 'apply'];
