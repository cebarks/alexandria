-- v010: the session `external_id` handle becomes account-local.
--
-- WHY THIS FILE EXISTS AT ALL, AND WHY IT CANNOT BE SPLIT FROM ITS READERS.
--
-- `external_id` is a *client-chosen* handle: pi, Claude Code and `store_memory(session_id: ...)` all
-- invent it, and two accounts working on the same repository in the same tool naturally produce the
-- same string. v005 defined `session_external_id_idx` UNIQUE over `external_id` alone, which is a
-- global namespace: the second owner's first store is answered with the *first* owner's session row,
-- and `touch` / `finalize` — both keyed on that handle with no owner in the WHERE clause — then write
-- another account's session prose. v009 added `session.account` deliberately but left the index
-- alone, and said why in its own comment: a store with a qualified index and unqualified readers
-- resolves — and then writes — somebody else's session. This file is that other half, and it is only
-- safe to apply together with the account-bound `SessionRepo` readers.
--
-- So the swap is atomic *by construction*: the global uniqueness is removed and the account-local
-- uniqueness is defined in the same migration, in the same multi-statement query, which the engine
-- runs as one implicit transaction. There is no intermediate state in which neither index exists,
-- because "no unique index at all" would let two rows for one account share a handle, which is a
-- correctness regression worse than the leak being fixed.
--
-- Replay safety (the rule `migration_test::test_replaying_a_completed_migration_is_not_an_error`
-- gates on, and the reason `migrate()` is not atomic across a file): every definition is
-- `DEFINE <kind> OVERWRITE` and every removal is `REMOVE <kind> IF EXISTS`. Note the qualifier goes
-- *after* the kind on this engine's grammar — `DEFINE OVERWRITE INDEX …` is a parse error.
--
-- Legacy rows: `session.account` is `option<record<account>>` (v009's compatibility rule — no
-- required-owner ASSERT before the Task 14 backfill), so a migration-pending store keeps its rows.
-- Such a row has `account = NONE` and is unreachable from every scoped reader in this repo, which
-- all carry a concrete bound account in their WHERE clause: `account = $account` cannot match `NONE`.
-- Unowned rows therefore stay migration-pending rather than becoming visible to the first caller who
-- happens to ask for their handle. Nothing here invents an owner for them.

-- The v005 global handle index. Removed *before* the qualified one is defined so the two never
-- coexist; both spellings are accepted on the pinned 3.3.0 engine and `IF EXISTS` is what makes a
-- replay of this file not an error on a store that already swapped.
REMOVE INDEX IF EXISTS session_external_id_idx ON session;

-- One handle per account, and only per account. `account` first so the pair is the key: two rows
-- with the same `external_id` under different accounts are two sessions, and two rows with the same
-- pair are impossible. This is also what makes the account-local total order in
-- `SessionRepo::list_scoped` a real total order — see the ordering contract restated there.
DEFINE INDEX OVERWRITE session_account_external_id_idx ON session FIELDS account, external_id UNIQUE;

-- `session.workspace` is written by the same path that stamps `account` and is what a scoped read
-- filters on, but it is not part of the unique key: the account *is* the namespace, and adding the
-- workspace would make the key depend on a column an owner can have exactly one of anyway. No index
-- is defined over it because nothing selects sessions by workspace without also naming the account.
DEFINE FIELD OVERWRITE account  ON session TYPE option<record<account>>;
DEFINE FIELD OVERWRITE workspace ON session TYPE option<record<workspace>>;
