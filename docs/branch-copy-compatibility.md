# Branch data compatibility

## Field preservation

Default `memory_merge` (`accept` / `replace`) and every `memory_apply` copy retain
both `subject_id` and `author_id`. This includes inactive historical records
copied by update, remove, restore and conflict acceptance. A historical row that
never had a subject remains `NULL`; copying a scoped row must not make it unscoped.

Similarity-based replacement only compares memories belonging to the same user
and subject. Two `NULL` subjects match; a `NULL` subject and a named subject do not.
The existing content-replacement strategy retains the target record's identity
and attribution; newly inserted records retain the source record's attribution.

## Historical schemas

When a historical snapshot branch lacks `subject_id`, Memoria adds it at the
same column position as the current main table. Before registration, it verifies
column names, order, types (including vector dimensions), nullability, defaults
and extra attributes. Equivalent implicit and explicit SQL NULL defaults are
accepted. Index creation remains best effort and is not a correctness gate.

Native diff, merge and pick also check the current schemas on each operation.
This protects previously registered branches whose startup migration reported
an incompatibility. Unsupported schema differences fail explicitly before the
native operation; existing branches are not deleted or silently rewritten.
Preserve/export their data before recreating a compatible branch. A newly created
incompatible snapshot clone is not registered and is cleaned up.

This is deliberately conservative across MatrixOne versions. MatrixOne 4.2.1
can handle some column-order differences, but that behavior is not assumed for
all supported builds. This guard does not apply to row restoration from a
snapshot, which uses explicit column-name mapping and current defaults.

Concurrent schema changes and branch/snapshot operations must be serialized;
the preflight schema check is not a lock against concurrent DDL.

## ALTER capability protection

Before a required branch-column migration, Memoria verifies the actual engine
behavior on two uniquely named disposable tables containing only synthetic data.
The probe includes parent/child ALTER, a subject index, native diff/merge with
field-value checks, and native deletion. It does not snapshot or copy user data
and does not rely on the server version string or a `_copy_` name in metadata.
The validated baseline is MatrixOne `4.2.1-d2393868a`; this is not a guarantee
that every build labelled 4.x has the same capability.

A failed probe stops required column migration before ALTER touches the user
branch. A newly created, unaltered historical clone is rejected and cleaned up
through the existing native branch deletion path. Existing branches are retained.
Optional index changes are skipped with a warning when capability is unverified.
Parent subject migration with registered branches and legacy compatibility DDL
are also checked, since altering the parent can affect its descendants.
Ordinary current-schema branches and non-migrating reads do not need a probe.

The service account needs CREATE/ALTER/DROP and native branch privileges to run
the probe. Permission errors and connectivity failures do not enable migration.
Concurrent callers on a store share the check; successful checks are cached for
five minutes and failures for thirty seconds, then retried. A process restart or
store database change resets the cache. Restart Memoria after an engine change;
do not downgrade or mix incompatible database builds during active migrations.

Request cancellation does not cancel the bounded probe/cleanup task. Cleanup
only targets exact, randomly generated names successfully created by that probe.
If an old engine turns its probe branch into a regular table, DROP TABLE is
allowed for that owned probe only, never as a generic user-branch delete fallback.
Cleanup failure cannot yield a successful capability check. A process/DB failure
or ambiguous CREATE response may still leave named `mem_lineage_probe_*` artifacts;
logs identify exact names for administrator inspection, not prefix-based deletion.
This protection does not repair branches already damaged by an earlier migration.

## Snapshot names

Unicode snapshot names retain their existing physical-name mapping. CREATE and
DROP quote the SQL identifier, while snapshot lookup continues to use its
existing name. No snapshot rename or user-data migration is required.

## Regression commands

Use a disposable MatrixOne server, not a production database. Each new E2E test
creates isolated databases and users. Set `DATABASE_URL` to that server and run
from `memoria/`:

```sh
SQLX_OFFLINE=true cargo test -p memoria-mcp --test branch_scope_e2e -- --test-threads=1
SQLX_OFFLINE=true cargo test -p memoria-mcp --test branch_e2e --test snapshot_e2e -- --test-threads=1
SQLX_OFFLINE=true cargo test -p memoria-git --test legacy_snapshot -- --test-threads=1
SQLX_OFFLINE=true cargo test -p memoria-storage --lib table_schema
SQLX_OFFLINE=true cargo test -p memoria-storage --lib branch_capability -- --include-ignored --test-threads=1
```

The scope suite covers ordinary and historical branches, all six apply-copy
statements, active/inactive records, nullable ownership, default merge, and
cross-subject similarity isolation. The legacy suite also verifies native-operation
rejection without data mutation and Unicode snapshot restore/delete.
