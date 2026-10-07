-- v008: dreaming / housekeeping (GitHub issue #43)
--
-- Five fields, no new tables: one middle rung of the demote -> quarantine -> soft-delete
-- ladder on `fact`, one second clock on `heat_state`, and four attribution columns on the
-- audit log. All `option<>` so every pre-existing row stays valid and `/debug/maintenance`
-- keeps rendering — a tightening that rejects existing data would fail at boot on real
-- corpora rather than in CI.
--
-- Every statement is `DEFINE ... OVERWRITE` (removals would be `REMOVE ... IF EXISTS`) because
-- `migrate()` is not atomic across a file: each statement commits in its own transaction and
-- `system_config` is stamped only after the whole file succeeds, so a crash mid-file leaves the
-- definitions applied with the old version stamp and the next boot re-runs this file from the
-- top. `test_replaying_a_completed_migration_is_not_an_error` is the gate on that rule. Note the
-- 3.2 grammar puts the qualifier *after* the kind: `DEFINE OVERWRITE FIELD` is a parse error.

-- Quarantine is the only genuinely new state: hidden from every read path, still inspectable by
-- an operator. Demote writes the existing `confidence`; delete writes the existing `deleted`.
--
-- The live predicate everywhere is `quarantined_at = NONE`. On this engine an absent option
-- field reads as NONE and BOTH `IS NOT NULL` and `!= NULL` are satisfied by NONE, so those
-- spellings filter nothing at all — pinned by `quarantine_none_predicate_pins_the_trap`.
DEFINE FIELD OVERWRITE quarantined_at ON fact TYPE option<datetime>;

-- `last_touched` anchors the stored `heat` value and moves on every materialisation (access,
-- spreading-activation warm, sweep). This is the second clock: the last time a caller actually
-- saw the row, which is the only input to the spacing ratio that grows stability. Sharing one
-- field between the two makes the sweep's hourly re-anchoring the spacing reference and
-- under-grows stability ~24x. NONE means "predates this column"; readers fall back to
-- `last_touched` for such rows, which is an upper bound on the true last access.
DEFINE FIELD OVERWRITE last_accessed_at ON heat_state TYPE option<datetime>;

-- The sweep pages `ORDER BY last_touched ASC LIMIT n` over a table with one row per memory.
-- Without this it is a full scan per tick.
DEFINE INDEX OVERWRITE idx_heat_state_last_touched ON heat_state FIELDS last_touched;

-- Every heat write is `UPDATE heat_state SET ... WHERE memory = type::record($id)`, and retrieval
-- issues ten of them per query. Measured before this index existed: ten batched access writes cost
-- 11.7ms against a 100-row corpus and 122ms against 1000 rows — a full scan per statement, on the
-- path every agent prompt takes. With it, 1.2ms at both sizes, flat in corpus size. Pinned by
-- `heat_lookups_by_memory_reach_the_index`, which asserts the plan rather than a duration.
-- Not UNIQUE: nothing in v001..v007 ever prevented two heat rows for one memory, and a unique index
-- on a corpus that may already contain duplicates would refuse to build rather than tell anyone.
-- Making it unique is a data-cleanup decision, not a schema declaration.
DEFINE INDEX OVERWRITE idx_heat_state_memory ON heat_state FIELDS memory;

-- Attribution. `run_id` is the unit of reversal (one tick that soft-deletes 40 rows is one event
-- a human wants to undo wholesale) and later the unit an admin would gate. `actor` is written
-- `system:dreaming`: dreaming is the one writer that can never produce a user identity, so a
-- column added after the fact would be backfilled with a guess.
DEFINE FIELD OVERWRITE run_id      ON maintenance_log TYPE option<string>;
DEFINE FIELD OVERWRITE actor       ON maintenance_log TYPE option<string>;
DEFINE FIELD OVERWRITE job         ON maintenance_log TYPE option<string>;
DEFINE FIELD OVERWRITE disposition ON maintenance_log TYPE option<string>;

-- What a value-changing job overwrote. `Appraise` lowers confidence, and without the prior number
-- stored alongside it a demotion pass cannot be reversed as a unit — `run_id` says which rows one
-- tick touched but not what they were before. Only value-changing verbs fill this (a split moves
-- edges, it overwrites no scalar), so it is NONE for the cluster rows by design.
DEFINE FIELD OVERWRITE previous_value ON maintenance_log TYPE option<float>;

-- `action` is the verb (split / merge / collapse / demote), `job` which scheduler job wrote the
-- row, `disposition` which rung of the ladder it landed on. Kept separate because they answer
-- different questions and only `disposition` is a closed ladder. Values live as constants in
-- `models::maintenance` so writers and readers cannot drift; no ASSERT here, because the
-- allowlist is a Rust-side contract and asserting on an optional column would have to accept
-- NONE as well as every value, which is a rule nobody can read at a glance.
DEFINE INDEX OVERWRITE idx_maintenance_log_job ON maintenance_log FIELDS job;
DEFINE INDEX OVERWRITE idx_maintenance_log_run ON maintenance_log FIELDS run_id;
