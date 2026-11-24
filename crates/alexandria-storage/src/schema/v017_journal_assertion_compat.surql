-- SurrealDB 4.0 cannot lower closures in field assertions under AUTH LIMIT.
-- Keep v009's journal invariants with scalar membership checks. No rows are rewritten.
DEFINE FIELD OVERWRITE expected_effects ON semantic_operation TYPE array<string> ASSERT array::len($value) > 0 AND NOT ('' INSIDE $value);
DEFINE FIELD OVERWRITE affected_scope_kinds ON semantic_operation TYPE array<string> DEFAULT [] ASSERT $value ALLINSIDE ['personal', 'project', 'unassigned'];
