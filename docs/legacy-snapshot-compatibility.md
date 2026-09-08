# Historical snapshots across schema upgrades

Related issue: [#246](https://github.com/matrixorigin/memoria/issues/246).

Snapshots created before `subject_id` was introduced retain the earlier memory
table schema. Upgrading the live table does not upgrade historical snapshots.

## Restore behavior

Restoration maps historical and current columns by name, not position. Added
columns use the current schema's defaults; historical memories receive a NULL
`subject_id`. A missing required column without a default is rejected.

Before modifying the current table, the server materializes the historical rows
in a private `mem_restore_<uuid>` table with the current schema and constraints.
The staging table retains the copied schema, constraints and indexes. This
includes FULLTEXT and IVF indexes: staging therefore incurs additional search
index writes. Removing copied search indexes via ALTER TABLE was not safe on the
3.0.11 historical-branch fixture (missing internal secondary-index tables), so
this optimization is deliberately not enabled. The regression suite instead
covers restoration with 512 vectors and real FULLTEXT/IVF indexes.
Source read failures, incompatible data and constraint failures leave current
rows untouched. Once preparation succeeds, DELETE and INSERT run in one database
transaction against live tables. An INSERT failure rolls back the DELETE.
Historical snapshot reads do not occur inside that transaction.

The database account needs CREATE/DROP TABLE privileges in the user's database,
and temporary capacity for a copy of the restored rows and their indexes. If
preparation cannot allocate storage or create the table, restoration fails
before deleting live rows.

Staging tables are cleaned after success and failure; request cancellation also
schedules cleanup. An abrupt process/database failure can leave a staging table
behind. Such a table is not a registered branch. Inspect cleanup warnings and
remove only confirmed abandoned staging tables, after checking that no restore
is still using them.

Atomicity is per restored table, not the entire multi-table memory/graph rollback.
Existing graph-table best-effort behavior is unchanged. Operators should retain
normal backups, serialize rollback operations and avoid concurrent writes while
restoring. A connection loss during COMMIT leaves the client uncertain whether
the complete transaction committed; do not blindly retry or delete data as
compensation.

The replacement is one transaction containing a whole-table DELETE and INSERT.
Peak memory, transaction write limits, disk/WAL space and timeouts depend on the
MatrixOne build, configuration, row/embedding sizes and live indexes. There is no
validated universal row-count limit. The indexed regression covers 512 vectors,
not production-scale capacity. Before restoring a large table, rehearse the same
snapshot size on a matching isolated deployment and provision headroom for both
staging and transaction writes. Do not split the replacement into independently
committed batches: that would lose the all-or-nothing guarantee.

## Offline recovery of abandoned tables

No online prefix-based deletion is enabled for `mem_restore_` or `br_` tables.
An unregistered table may belong to an in-flight restore or a branch not yet
registered. The existing sandbox reaper must not be expanded to these prefixes.

The `restore_cleanup` example provides an explicit offline fallback. Set
`DATABASE_URL` securely to **one affected user database**, not the system database.
Its default mode only lists exact generated names and their registration status:

```sh
cd memoria
SQLX_OFFLINE=true cargo run -p memoria-git --example restore_cleanup
```

Before deleting anything, stop **all** Memoria API replicas, background workers,
MCP processes and any other writers connected to that database; wait for their
database operations to end. Inspect the candidate and retain a backup if needed.
Then select one exact table printed by the inspection:

```sh
SQLX_OFFLINE=true cargo run -p memoria-git --example restore_cleanup -- \
  --delete EXACT_TABLE_NAME --all-writers-stopped
```

`--all-writers-stopped` is an operator confirmation, not an automatic fence.
Never schedule this command as an online job. It rejects unexpected names,
protects branches registered in **any** status, rechecks registration before
deletion, and fails closed when the registry cannot be read. Deletion has no
automatic undo; recovery requires a retained backup or snapshot.

## Branches from historical snapshots

Before registering a newly cloned historical branch, Memoria ensures that
`subject_id` and every column in the live `mem_memories` schema exist. This is a
table-shape check, not the historical-row default rule: current reads and writes
also reference nullable/defaulted columns such as `embedding`, `updated_at` and
`trust_tier`. Those missing columns must not be silently accepted. Only the known
`subject_id` migration is applied automatically; other missing columns require
an explicit compatible migration rather than guessed values. Column migration failures are
returned and cleanup of the unregistered clone is attempted. Creating the subject
index is best-effort: an index error is logged without rejecting a usable branch.
Startup migration and historical-clone migration share this policy; startup
continues to log errors on existing branches rather than failing all users.
Creating branches from current
data retains its existing behavior.

This column-presence check does not normalize column order or types. MatrixOne's
native branch diff has a stricter schema-equivalence requirement and may reject
a historical clone whose schema differs from the live table, even when Memoria's
explicit-column reads/writes and merge work. Passing this check is not a guarantee
of native diff compatibility.

New physical branch names are ASCII while `mem_branches.name` retains the user's
original display name, including Chinese. Normal creation and creation from a
snapshot use the same physical-name generator. Existing Unicode physical names
remain valid for migration, access and offline cleanup, with SQL identifier
quoting. Snapshot-name generation is unchanged to preserve historical mappings.

## Database compatibility boundaries

SQLx obtains historical column metadata using PREPARE on a SELECT with
`{SNAPSHOT = '...'}`. This is an explicit database capability dependency, verified
on the 3.0.11 and 3.0.15 builds below, not a claim of support for every MO version.
Snapshot reads remain outside the write transaction. Callers must serialize
snapshot operations and quiesce writes (historical MO#23860 / MO#23861 concerns).

Known 3.0.11 schema-migration limitation: adding a column to a table with UNIQUE,
FULLTEXT and IVF indexes can make an otherwise working FULLTEXT query fail with
`invalid input: column word does not exist`, **before any restore was called**.
This was reproduced with SQL alone; the equivalent indexed upgrade fixture
passed on 3.0.15. This change does not fix
MatrixOne or certify that upgrade path on 3.0.11. Rehearse migrations of indexed
production tables separately; do not treat successful restore tests as approval
for a direct production upgrade. The indexed restore regression starts from a
schema with working search indexes; separate legacy tests cover added columns.

## Regression checks

### Production upgrade target: MatrixOne 4.2.1

The production candidate is
`shanghai.idc.matrixorigin.cn:30019/mocloud/matrixone:v4.2.1-d2393868a-2026-08-28`,
not 3.0.15. The older builds below are historical regression baselines only.

An isolated local upgrade rehearsal on 2026-09-08 used:

- Target image digest:
  `sha256:dab6628f0a71f53b6ec347a6264ed9f1bf72cf1cb47f0e451f377fe9ed243cff`.
- Source: MatrixOne 3.0.11, SQL-reported commit `6a0394c`.
- Unchanged Memoria image: `apiserver-20260511-27107f5-df643526` (0.4.0).
- Synthetic multi-database users, API keys, memories, a snapshot, a branch, and
  a separate 512-vector fixture with UNIQUE, FULLTEXT and IVF indexes.
- Linux/amd64 images under local emulation, not a production performance test.

**The in-place upgrade is not approved by this rehearsal.** The target returned
`8.0.30-MatrixOne-v4.2.1` / `d2393868a`; existing rows, direct historical snapshot
reads, fulltext search and nearest-neighbor queries worked. However, asynchronous
catalog migration repeatedly failed while adding `kind` to `mo_catalog.mo_pitr`:

```text
no such table mo_catalog.mo_feature_registry
```

The final internal catalog version `4.0.6`, offset `5`, remained in state `0`,
with the first `3.0.2 -> 4.0.0` migration incomplete. These are internal catalog
versions, not the server release tag. `SHOW SNAPSHOTS` failed with
`invalid input: column kind does not exist`, and the unchanged old API's snapshot
path returned HTTP 500. An open SQL port, a successful `SELECT VERSION()`, or
Memoria's `/health` response therefore does not establish upgrade readiness.
The new Memoria regression suites have **not** been validated on this target.

Before production rollout, resolve the catalog-upgrade failure through the
MatrixOne-supported upgrade procedure or an approved corrected build, then repeat
the old-Memoria acceptance checks before deploying new Memoria. Do not manually
create system tables or assume an untested intermediate release fixes the issue.
No MatrixOne source or production deployment was changed by this test.

Deployment preconditions for repeating the rehearsal:

- Preserve LogService identity, including its effective hostname; reusing the
  data volume under a different hostname can produce `shard not bootstrapped`.
- Preserve the old file-service backend and paths. This target's bundled
  quickstart opts into `DISK-V2` for fresh deployments; the rehearsal reused the
  old launch configuration instead of changing the storage backend.
- Keep a pre-upgrade data copy. The local 3.0.11 process exceeded the 30-second
  stop timeout and exited with code 137; this run includes recovery after forced
  termination, not a certified graceful-shutdown or cluster rolling-upgrade test.

### Historical regression baselines

Use a disposable MatrixOne instance, not a production database. The new legacy
tests create isolated databases. Run against both 3.0.11 and 3.0.15, selecting the
appropriate local port in `DATABASE_URL`:

For local verification, start dedicated test instances first (these ports are
not assumed to be permanent services; do not substitute an existing user stack):

```sh
docker run -d --name memoria-snapshot-test3011 -p 127.0.0.1:16011:6001 matrixorigin/matrixone:3.0.11
docker run -d --name memoria-snapshot-test3015 -p 127.0.0.1:16015:6001 matrixorigin/matrixone:3.0.15
```

Wait until each instance accepts SQL connections before running tests. A refused
connection or pool timeout means the test did not reach database verification.

```sh
cd memoria
export SQLX_OFFLINE=true
export DATABASE_URL=mysql://root:111@127.0.0.1:16011/test
export EMBEDDING_DIM=8
cargo test -p memoria-git --test legacy_snapshot -- --test-threads=1
cargo test -p memoria-git --lib failed_insert_rolls_back_preceding_delete -- --ignored --nocapture
cargo test -p memoria-git --example restore_cleanup -- --include-ignored --nocapture
cargo test -p memoria-mcp --test branch_e2e --test snapshot_e2e -- --test-threads=1
```

Coverage includes reordered/new columns and defaults, populated and empty legacy
snapshots, repeated restore, missing snapshots/required columns, incompatible
constraints, failure after DELETE, and MCP branch read/write plus rollback after
the schema migration. Additional regressions cover 512 non-null embeddings with
real IVF/FULLTEXT indexes and constraint preservation,
restoring duplicate rows in tables without unique keys, real DDL permission
failures (optional index versus required column), preservation
of the original error when rollback fails, Unicode branch names (new and legacy),
required/nullable/defaulted columns missing from historical clones, and offline
cleanup safety for both ASCII and Unicode table names. The database failure-injection test is explicitly invoked
by CI; it is ignored by the database-free unit-test command.

After local testing, stop and remove only the dedicated instances above. Their
disposable database contents are deleted with the containers:

```sh
docker stop memoria-snapshot-test3011 memoria-snapshot-test3015
docker rm memoria-snapshot-test3011 memoria-snapshot-test3015
```

Local verification on 2026-09-08 passed against isolated official images:

| MatrixOne | Build commit | Restore/branch compatibility | Transaction failure | Offline cleanup safety | MCP branch tests | MCP snapshot tests |
| --- | --- | --- | --- | --- | --- | --- |
| 3.0.11 | `6a0394c` | 9 | 1 | 1 | 49 | 21 |
| 3.0.15 | `43e871c` | 9 | 1 | 1 | 49 | 21 |

Seven focused database-free tests also cover Send compatibility, missing-runtime
Drop, original-error preservation, offline cleanup argument/name validation,
ASCII branch suffixes without changing snapshot mappings, and the distinction
between missing historical values and missing physical columns.
The storage `branch_ops` and `subject_id_mo_compat` suites also passed on each
build (10 tests each), including startup migration idempotency and scoped reads.

The original DELETE/SELECT-star sequence was reproduced only on disposable test
data and left its table empty after a column-count error. The fixed implementation
successfully restored that same fixture. This verifies the snapshot fix, not the
entire production upgrade or rolling-deployment compatibility.
