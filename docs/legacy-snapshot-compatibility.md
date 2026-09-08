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

## Branches from historical snapshots

Before registering a newly cloned historical branch, Memoria ensures that
`subject_id` and its composite index exist. Migration failures are returned and
cleanup of the unregistered clone is attempted. Creating branches from current
data retains its existing behavior.

## Regression checks

Use a disposable MatrixOne instance, not a production database. The new legacy
tests create isolated databases. Run against both 3.0.11 and 3.0.15, selecting the
appropriate local port in `DATABASE_URL`:

```sh
cd memoria
export SQLX_OFFLINE=true
export DATABASE_URL=mysql://root:111@127.0.0.1:16011/test
export EMBEDDING_DIM=8
cargo test -p memoria-git --test legacy_snapshot -- --test-threads=1
cargo test -p memoria-git --lib failed_insert_rolls_back_preceding_delete -- --ignored --nocapture
cargo test -p memoria-mcp --test branch_e2e --test snapshot_e2e -- --test-threads=1
```

Coverage includes reordered/new columns and defaults, populated and empty legacy
snapshots, repeated restore, missing snapshots/required columns, incompatible
constraints, failure after DELETE, and MCP branch read/write plus rollback after
the schema migration. The database failure-injection test is explicitly invoked
by CI; it is ignored by the database-free unit-test command.

Local verification on 2026-09-08 passed against isolated official images:

| MatrixOne | Build commit | Legacy restore tests | Transaction failure | MCP branch tests | MCP snapshot tests |
| --- | --- | --- | --- | --- | --- |
| 3.0.11 | `6a0394c` | 6 | 1 | 46 | 21 |
| 3.0.15 | `43e871c` | 6 | 1 | 46 | 21 |

The original DELETE/SELECT-star sequence was reproduced only on disposable test
data and left its table empty after a column-count error. The fixed implementation
successfully restored that same fixture. This verifies the snapshot fix, not the
entire production upgrade or rolling-deployment compatibility.
