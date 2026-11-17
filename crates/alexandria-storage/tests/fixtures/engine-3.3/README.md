# Frozen SurrealDB 3.3 physical fixture

**Never engine-open `store/` in place.** The integration test takes a guarded
physical copy and opens only that disposable target. `LOCK` is an inert captured
file; the test acquires its own read-only lock and verifies the source bytes.

This fixture was written by a separate process built with SurrealDB **3.3.0**
and SurrealKV **0.21.4**, using the application schema at commit
`c62edc3bb533f13dd54271ee5a264de70ce59977`. `writer.txt` records those versions;
`logical.json` captures every application table and row before any 4.x open.

The corpus includes sixteen identical live vectors, deleted and quarantined
facts, a stored centroid, embedding identity and token budget, nonempty metadata,
provenance/document/cluster/session edges, a session, heat state, and a reminder.
The fixture-only `metadata.sentinel` child fields are explicitly declared because
the application tables are schema-full. No model or inference is required.

`writer.rs` is the original 3.3-only integration-test generator, retained for
reproducibility; it is not compiled by the current workspace and is not a
migration command. It asserts its compile-time lockfile contains SDK 3.3.0.
To regenerate, set `LEGACY_TREE` to a new isolated worktree path and
`NEW_FIXTURE` to a new absolute output directory, then from the current checkout:

```sh
: "${LEGACY_TREE:?Set a new legacy worktree path}" "${NEW_FIXTURE:?Set a new fixture path}"
git worktree add --detach "$LEGACY_TREE" c62edc3bb533f13dd54271ee5a264de70ce59977
cp crates/alexandria-storage/tests/fixtures/engine-3.3/writer.rs \
  "$LEGACY_TREE/crates/alexandria-storage/tests/engine_upgrade_test.rs"
cd "$LEGACY_TREE"
ALEXANDRIA_UPGRADE_FIXTURE="$NEW_FIXTURE" cargo test --locked \
  -p alexandria-storage --test engine_upgrade_test write_legacy_fixture \
  -- --ignored --exact
```

Use the legacy checkout's own target directory, not another worktree's build
cache. Wait for that separate process to exit before copying the resulting
fixture. Preserve the original input and review regenerated physical files and
logical JSON together; timestamps and the captured process ID naturally change.
