# Idempotent single-memory writes

`POST /v1/memories/idempotent` accepts the normal single-store body plus a required
`idempotency_key` containing 1–128 ASCII graphic bytes. It uses the same scoped
memory-write permission, owner-only deployment authentication, branch validation,
sensitivity checks and user database routing as ordinary memory writes.

The authenticated owner and resolved storage branch scope the key. An omitted or
blank branch follows the current checkout; specifying that same branch explicitly
replays the same receipt. The operation freezes the routed SQL store and branch
table before embedding, so a concurrent checkout cannot retarget the insert or
conflict lookup. An invalid/deleted branch fails instead of silently moving a keyed
write to another branch. Clients needing a stable scope across separate retries
must specify it explicitly (for example, `"branch": "main"`).

Clients cannot choose another
owner through JSON. Successful retries return the same persisted memory ID.
Concurrent requests use the memory primary key to arbitrate one insertion;
only the insertion winner reports a new memory and schedules entity extraction.
The idempotent path bypasses approximate vector deduplication so two different
session operations cannot acquire each other's record or session metadata.

A key reused with a different content, type, session, trust tier, confidence,
observed time, author, subject or caller metadata fails. The backend preserves a
request fingerprint in reserved `_memoria_store_request_sha256` metadata; callers
cannot set or query that reserved field, and API memory responses hide it while
returning caller metadata unchanged. Payload fingerprints canonicalize nested
JSON object keys. Ordinary vector dedup excludes receipt-bearing rows, so an
ordinary store cannot supersede a receipt or replace its metadata. A retry never
reactivates an inactive receipt. The guarantee depends on retaining the record/tombstone;
physical database or record erasure ends that receipt's lifetime. Account/branch
retention and deletion policies remain authoritative.

Protocol v1 encodes `["memoria.store.v1", owner, resolved_branch, key]` as compact
JSON, hashes it with SHA-256 and uses the first 16 bytes as a lowercase 32-character
hex memory ID. A caller can reject a successful response with an incompatible ID
before treating it as a write receipt. The identity is an operation key, not an
authorization credential. Normal single-store calls may also supply the optional
key; batch store rejects keys because it has no corresponding atomic batch
receipt contract.

The dedicated path allows callers such as Astra's durable session-memory journal
to fail before writing when a deployment uses an old backend. They must retain
source state and retry/reconcile that same frozen payload. They must not fall back
to ordinary store on 404, 405, a malformed receipt or a lost response.

Example (the token remains in the application's credential boundary):

```json
{
  "content": "[@episode/summary] Session completed its task",
  "memory_type": "episodic",
  "session_id": "session-example",
  "trust_tier": "T3",
  "branch": "main",
  "idempotency_key": "session-example:episode:revision-1"
}
```

Astra must deploy this backend endpoint before enabling its matched durable
finalization client. Repeated exit/close, process restart and partial-stage retry
can then reuse receipts without manufacturing duplicate episode or scene nodes.
