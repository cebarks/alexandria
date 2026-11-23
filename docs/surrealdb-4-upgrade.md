# Offline SurrealDB 3.3 → 4.0.1 upgrade

The operator receipts below were collected on the main-branch engine upgrade, not on an
installed multi-user/refinement store. They do not authorize live migration or cutover of
the combined feature.

This is a **Linux, offline, copy-first operator procedure**, not an automatic
Alexandria migration or a cutover command. It was exercised with the official
3.3.0 and 4.0.1 Linux amd64 operators and a separate-process 3.3 application
fixture. The current application pins the official SDK family to **4.0.1**;
its resolved SurrealKV backend is **1.2.0**, matching the published 4.0.1 lock.

## Stop conditions and rollback

- Stop every writer before taking a verified physical backup of the entire data
  directory. The default is `$XDG_DATA_HOME/alexandria/data`, or `/data/db` in the
  container. Retain that backup and the old application binary independently of
  this exercise. Work only on disposable copies of the backup; **never open the
  original or backup with 4.x**, including as a lock probe.
- Ordinary 4.0.1 startup refuses unconverted 3.x SurrealKV files. Experimental
  physical conversion must be explicitly admitted on a copy. It can advance the
  datastore's feature level and writes new engine metadata and file formats.
  **A target touched by 4.x is not readable rollback material for stock 3.3, even
  if held at Legacy feature level.** Rollback means the untouched backup plus the
  old binary; writes accepted after a later cutover need separate reconciliation.
- This procedure upgrades the physical engine, **not** the embedding space.
  Preserve the model identity, effective token budget, exact vectors and stored
  centroids. Do not re-embed, rewrite centroids, clear identity locks, or use
  `migrate-embeddings --force` to make a format conversion pass.
- Abort on any copy, conversion, record comparison, index, identity, or query
  failure. Preserve the failed copy and logs. Do not retry on the backup or
  silently substitute export/import. Nothing here authorizes production cutover.
- The fixture uses application schema v008. Inventory your own database and
  account for its schema and any additional application tables before comparing.
  Keep application schema upgrades separate from this engine-format comparison.
  macOS crypto dependency graphs were inspected, but this filesystem/lock and
  conversion procedure has **not** been runtime-validated on macOS or Windows.

## 1. Prepare private copies and receipts

Requirements: Python 3.11+, GNU `cp`, `diff`, `flock`, the exact official
version-specific operators, and enough free space for two full working copies
in addition to the retained backup. Verify downloaded binaries against the
publisher's release-asset SHA-256, then check `version`. The tested Linux amd64
archives were:

| Archive | Publisher-verified SHA-256 |
| --- | --- |
| `surreal-v3.3.0.linux-amd64.tgz` | `44aeab565f7e7e39d2d0bf0583c8aae648babc91373c70b2658288d95bbbcd55` |
| `surreal-v4.0.1.linux-amd64.tgz` | `9d9d26313bf3f1aeffd066def9493932c79053fc9bca8f1b39f83d511f65577d` |

Set `BACKUP` to your **already verified, stopped 3.3 backup**, not the original
store, `WORK` to a new absolute private workspace, and `SURREAL3`/`SURREAL4` to
absolute binary paths. Do not reuse existing working directories. Commands below
belong to the same dedicated POSIX shell (not a long-lived interactive service shell):

```sh
set -eu
umask 077
: "${BACKUP:?Set the offline backup path}" "${WORK:?Set a new private workspace}"
: "${SURREAL3:?Set the 3.3.0 binary}" "${SURREAL4:?Set the 4.0.1 binary}"
"$SURREAL3" version
"$SURREAL4" version
test ! -e "$WORK"
mkdir -m 700 "$WORK"
INSPECT3="$WORK/inspect3"
TARGET="$WORK/converted4"

test -f "$BACKUP/LOCK" && test ! -L "$BACKUP/LOCK"
exec 3<"$BACKUP/LOCK"
flock -n -x 3
```

The read-only file descriptor does not truncate `LOCK`. This is a cooperative
Linux lock, **not** protection against noncooperating writers or local filesystem
attackers. The backup must already be offline and under your control. Do not use
an engine open as a lock test: even a rejected open can truncate its lock file.

Define a one-off content receipt that rejects symlinks and special files and
includes **every** file, including root and nested `LOCK` files:

```sh
manifest() {
    python3 - "$BACKUP" <<'PY'
import hashlib, json, pathlib, stat, sys
root = pathlib.Path(sys.argv[1])
assert root.is_absolute() and root.is_dir() and not root.is_symlink()
result = {}
for p in sorted(root.rglob('*')):
    mode = p.lstat().st_mode
    if stat.S_ISDIR(mode):
        continue
    assert stat.S_ISREG(mode), f'Not an ordinary file: {p}'
    with p.open('rb') as f:
        result[str(p.relative_to(root))] = hashlib.file_digest(f, 'sha256').hexdigest()
print(json.dumps(result, sort_keys=True))
PY
}
manifest > "$WORK/source-before.json"
mkdir -m 700 "$INSPECT3" "$TARGET"
cp -a --reflink=never "$BACKUP/." "$INSPECT3/"
cp -a --reflink=never "$BACKUP/." "$TARGET/"
diff -qr --no-dereference "$BACKUP" "$INSPECT3"
diff -qr --no-dereference "$BACKUP" "$TARGET"
```

## 2. Capture the 3.3 logical baseline

Only the **3.3** binary may open `INSPECT3`. Use `INFO FOR DB STRUCTURE;` through
that operator first to inventory tables. Write `snapshot.surql` in `WORK` with
these statements for schema v008, adding a statement for every additional
application table you found. Keep the same file and statement order for both
snapshots; do not filter out hidden rows or vector fields.

```sql
SELECT * FROM cluster ORDER BY id;
SELECT * FROM contains_cluster ORDER BY id;
SELECT * FROM contains_memory ORDER BY id;
SELECT * FROM contains_session_memory ORDER BY id;
SELECT * FROM embedding_config ORDER BY id;
SELECT * FROM extracted_from ORDER BY id;
SELECT * FROM fact ORDER BY id;
SELECT * FROM has_provenance ORDER BY id;
SELECT * FROM heat_state ORDER BY id;
SELECT * FROM maintenance_log ORDER BY id;
SELECT * FROM memory_edge ORDER BY id;
SELECT * FROM provenance ORDER BY id;
SELECT * FROM raw ORDER BY id;
SELECT * FROM reminder ORDER BY id;
SELECT * FROM session ORDER BY id;
SELECT * FROM system_config ORDER BY id;
```

```sh
env -i PATH="$PATH" HOME="$HOME" "$SURREAL3" sql \
  --endpoint "surrealkv://$INSPECT3" --namespace alexandria --database default \
  --json --hide-welcome < "$WORK/snapshot.surql" > "$WORK/before.log"
```

Check the command output for statement/parse errors even if the old CLI exits
zero: **3.3 can report a parse error with exit code 0**. Require exactly one
successful result per statement. Wait for the process to exit before proceeding.
Snapshots contain complete memory content and vectors: keep them private.

## 3. Convert only the new target and rebuild HNSW

Use a free loopback port (below: 18081) and a new disposable operator password.
Root authentication stays enabled. The temporary root credential is persisted
in the converted store's engine metadata; it is not an application memory or a
production service credential. Do not expose this listener to the network.

```sh
cleanup() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill -INT "$SERVER_PID" 2>/dev/null || :
        wait "$SERVER_PID" || :
    fi
}
trap cleanup 0
trap 'exit 130' INT
trap 'exit 143' TERM
OP_PORT=18081
OP_USER=upgrade_operator
OP_PASS="$(python3 -c 'import secrets; print(secrets.token_urlsafe(32))')"
ENDPOINT="http://127.0.0.1:$OP_PORT"
env -i PATH="$PATH" HOME="$HOME" \
  SURREAL_USER="$OP_USER" SURREAL_PASS="$OP_PASS" \
  SURREAL_EXPERIMENTAL_UPGRADE_FROM_3X=true \
  "$SURREAL4" start --bind "127.0.0.1:$OP_PORT" --no-defaults \
  "surrealkv://$TARGET" > "$WORK/server.log" 2>&1 &
SERVER_PID=$!
```

Wait for the **new process's** startup/health readiness, and check `server.log`
for successful conversion and the 3.3.0 → 4.0.1 datastore-version advance. Abort
on bind errors; a response from an unrelated process occupying the port is not
readiness. Confirm the owned process is still running and its log reports the web
server listening on the selected address before sending credentials. Never send
an unrelated process your credentials. Keep the server isolated
and stop it on failure as well as success.

**The environment flag is forwarded by `surreal start`, not by embedded
`surreal sql`.** A direct `sql --endpoint surrealkv://...` with that environment
variable still refuses the old store: its local SDK configuration does not read
these datastore environment options. Do not work around this by enabling a
process-global upgrade setting in Alexandria.

Run each of these SQL statements through the authenticated remote operator.
`--multi` is required for the multiline query examples below: without it the CLI
can execute an incomplete line before its `WHERE` clause arrives.

```sh
sql4() {
    env -i PATH="$PATH" HOME="$HOME" \
      SURREAL_USER="$OP_USER" SURREAL_PASS="$OP_PASS" \
      "$SURREAL4" sql --endpoint "$ENDPOINT" \
      --namespace alexandria --database default --json --hide-welcome --multi
}
printf '%s\n' 'REBUILD INDEX fact_embedding_hnsw ON fact;' | sql4 > "$WORK/rebuild.log"
printf '%s\n' 'INFO FOR INDEX fact_embedding_hnsw ON fact;' | sql4 > "$WORK/index.log"
sql4 < "$WORK/snapshot.surql" > "$WORK/after.log"
```

A successful `DEFINE INDEX IF NOT EXISTS` is **not** a rebuild. Inspect the
rebuild result and require the index status to be `ready`; do not treat the
operator process merely exiting as proof. HNSW files from the old engine must be
rebuilt even though embeddings and centroids remain unchanged. Ordinary startup
does not blindly rebuild every index.

## 4. Compare data and demonstrate indexed retrieval

The JSON CLI emits **an envelope per statement**, potentially on multiple
lines. Do not parse just its last output line. The following canonicalization
compares all successful table results, preserving float representation such as
signed zero and including every field. Set `TABLES` to the number of statements
in your reviewed snapshot file (16 for the unmodified v008 file above).

```sh
TABLES=16
python3 - "$WORK/before.log" "$WORK/after.log" "$TABLES" <<'PY'
import json, pathlib, sys
expected = int(sys.argv[3])
def canonical(name):
    results = []
    for line in pathlib.Path(name).read_text().splitlines():
        if not line.startswith('['):
            continue
        value = json.loads(line)
        assert isinstance(value, list)
        results.extend(value)
    assert len(results) == expected, (name, len(results), expected)
    assert all(isinstance(rows, list) and all(isinstance(r, dict) for r in rows)
               for rows in results), 'Expected SELECT record arrays, not errors'
    return json.dumps(results, sort_keys=True, separators=(',', ':'), allow_nan=False)
assert canonical(sys.argv[1]) == canonical(sys.argv[2]), 'Application data changed'
print('All application table results match')
PY
```

Also inspect the embedding-space/token-budget keys in `system_config` and any
migration state in `embedding_config`: pending/corrupt embedding migrations are
a stop condition, not something this procedure repairs. Full comparisons can
be large: scans and snapshots cost O(total application data); the hashing step
streams files, while JSON comparison needs memory proportional to the snapshots.

Use a known finite, correctly dimensioned query vector from your corpus in a
filtered indexed query, and explain **that same query**:

```sql
SELECT id FROM fact
WHERE deleted = false AND quarantined_at = NONE
  AND embedding <|18,150|> [1.0f, 0.0f];
EXPLAIN FORMAT JSON SELECT id FROM fact
WHERE deleted = false AND quarantined_at = NONE
  AND embedding <|18,150|> [1.0f, 0.0f];
```

The two-dimensional vector and `k=18` above are the fixture example, **not** a
query for a normal 384/768-dimensional deployment. Substitute your dimensions
and choose a breadth above your requested `k`. Require `KnnScan` naming
`fact_embedding_hnsw`, unique expected records and no deleted/quarantined rows.
A plausible result list alone does not prove the index is used. In 4.0,
approximate KNN without an index errors; a named-distance query is brute force
and is not an acceptable substitute for this check. Inventory tables again to
ensure the snapshot did not omit a table.

## 5. Stop, recheck the backup, and leave cutover explicit

```sh
kill -INT "$SERVER_PID"
wait "$SERVER_PID"
SERVER_PID=
manifest > "$WORK/source-after.json"
cmp "$WORK/source-before.json" "$WORK/source-after.json"
exec 3<&-
unset OP_PASS
trap - 0 INT TERM
```

Require graceful server shutdown. On errors, stop the owned temporary process
and preserve logs/copies; do not leave it running accidentally. A converted
copy was separately reopened with **default** 4.0.1 embedded settings, no
experimental flag, and all application data still compared equal. Thus the
experimental flag is a one-time conversion admission, not an application
startup requirement after successful conversion.

Only after all checks pass may an operator separately decide when and how to
point the new application at the converted copy. Keep the matching embedding
configuration and backup/old binary. This document performs no deployment change
and promises no rollback of post-cutover writes.

## Regression evidence

`engine_upgrade_test` uses the frozen
[3.3 fixture](../crates/alexandria-storage/tests/fixtures/engine-3.3/README.md),
`OfflineStoreGuard` and the existing storage/index verification helpers. It
asserts default connection refusal, explicit copy-only SDK admission, exact
application rows and vector/centroid bits, mandatory rebuild/readiness and real
HNSW plan, sixteen unique identical live vectors (above the old index's small
posting threshold), hidden-row exclusion, and unchanged source payload/LOCK.
The original writer was a separate 3.3 process, not a 4.x-created fixture.

Independent official-operator checks covered the physical version advance,
strict canonical equality of all sixteen table results before/after conversion,
authenticated loopback operation, graceful shutdown, and the subsequent default
4.0.1 reopen. These are fixture-scale storage proofs, not a claim that every
production corpus, operating system, or storage backend has been validated.
