# Snapshot list latency and optional counts

`GET /v1/snapshots` supports these optional query parameters:

| Parameter | Default | Behavior |
| --- | --- | --- |
| `limit` | `20` | Number of snapshots in the page. |
| `offset` | `0` | Offset within the filtered list. |
| `include_safety` | `true` | Set to `false` to exclude unregistered automatic safety snapshots before pagination and summary generation. Registered user snapshots are retained, including names beginning with `pre_`. Milestone snapshots remain visible. |
| `resolve_memory_counts` | `true` | Set to `false` to use only counts already stored in snapshot metadata. Missing counts are returned as `null`; no historical count query or count-backfill write is performed. Known counts, including zero, remain integers. |

For a dashboard that does not display safety snapshots, request:

```http
GET /v1/snapshots?include_safety=false&resolve_memory_counts=false
```

`total`, `offset`, `limit`, `has_more`, and the textual `result` describe the
filtered list. A missing count means **unknown**, not an empty snapshot. Clients
must tolerate `memory_count: null` when opting out of count resolution.

Without these options, existing behavior is preserved: safety snapshots remain
visible, and missing counts are resolved through historical snapshot queries.
Registered legacy snapshots still receive read-through count backfill in that
mode. Unregistered safety snapshots do not have a registration in which to store
that count, so repeated exact-count requests can repeat historical queries.

A legacy registered snapshot with no cached count stays `null` for every
cached-only request until another exact-count request or a separately managed
backfill populates its metadata. Fast listing does not schedule background
backfill. Any bulk backfill should be a bounded maintenance operation outside
the dashboard request path, with its own authorization and performance review.

Safety classification is attached to `VisibleSnapshot` during discovery. The
list route consumes that classification instead of interpreting physical-name
prefixes again; registered snapshots are classified as user snapshots even when
their names resemble a safety snapshot.

Historical reads can require loading index snapshot state and checkpoint
metadata in MatrixOne. Fast listing avoids that work for counts; it still reads
the snapshot catalog and registration metadata and is not a guarantee that every
database operation completes within a particular deadline.

Snapshot creation, detail, rollback, authorization, and scope isolation are
unchanged. Deploy support for these parameters in Memoria before switching a
website client to the fast mode; older servers may silently ignore unknown query
parameters.
