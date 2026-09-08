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
```

The scope suite covers ordinary and historical branches, all six apply-copy
statements, active/inactive records, nullable ownership, default merge, and
cross-subject similarity isolation. The legacy suite also verifies native-operation
rejection without data mutation and Unicode snapshot restore/delete.
