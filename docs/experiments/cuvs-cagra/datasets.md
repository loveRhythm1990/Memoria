# Synthetic datasets and reproduction

All inputs are artificial. No customer memories or private benchmark-provider data are included.

## Dataset A: exact SQL neighbors

[`datasets/sql-vectors-8d.jsonl`](datasets/sql-vectors-8d.jsonl) has 20 rows. Row i has ID i and vector `[i,i,i,i,i,i,i,i]`, for i=1..20.

Create a fresh test database and table `smoke(id BIGINT PRIMARY KEY, v VECF32(8))`, then insert the fixture rows. Query vector `[7,7,7,7,7,7,7,7]` before indexing; the nearest ID must be 7.

```sql
SET experimental_cagra_index = 1;
SET cagra_threads_build = 2;
SET cagra_max_index_capacity = 128;
CREATE INDEX smoke_cagra USING cagra ON smoke(v)
op_type 'vector_l2_ops'
intermediate_graph_degree=16 graph_degree=8 itopk_size=32;

EXPLAIN SELECT id FROM smoke
ORDER BY l2_distance(v, '[7,7,7,7,7,7,7,7]') LIMIT 1;
```

Check for `cagra_search`; repeat exact probes for IDs 1, 7, 10, 15, and 20. Then test index maintenance:

| Action | Query vector | Expected nearest ID |
| --- | --- | --- |
| INSERT ID 21 with `[21]*8` | `[21]*8` | 21 |
| UPDATE ID 21 to `[-0.5]*8` | `[-0.5]*8` | 21 |
| DELETE ID 21 | `[-0.5]*8` | 1 |

The table uses explanatory Python notation; SQL needs an explicit eight-element vector. Poll once per second with a bounded timeout (90 seconds in the acceptance run).

## Dataset B: Memoria API memories

[`datasets/memoria-memories.jsonl`](datasets/memoria-memories.jsonl) contains the 20 exact Chinese texts used in the API run, with stable fixture IDs, memory type, subject, confidence, and session labels. Ten are fictional preferences and ten are fictional project conventions.

[`datasets/memoria-extra-cases.json`](datasets/memoria-extra-cases.json) contains user B's marker, the post-index new memory, correction text, and deletion target.

Use fresh users A and B and prefix session labels with a unique run ID. Record API-returned memory IDs by fixture ID. The run used BGE-M3/1024-dimensional embeddings. Generated float vectors are not pinned fixtures: exact vector reproducibility also depends on the embedding endpoint/model revision and numerical implementation.

### Reproduction sequence

1. POST 20 memories as A and the marker as B. Verify 20 non-null embeddings in A's table, an internal signed auto-increment BIGINT key, and unique string business IDs.
2. Retrieve `memory-01` from another session before indexing.
3. In A's database, create CAGRA on `mem_memories.embedding` using [the explicit script](../../../scripts/cuvs/create_cagra_index.sql).
4. Repeat retrieval. Check target recall, vector branch activity, graph inactivity, and SQL index selection independently.
5. Write the incremental memory. Read its stored vector and poll a direct nearest-neighbor SQL query until the new ID is nearest. Also retrieve it through the API in a new session. The direct SQL check prevents fulltext fallback from hiding a stale index.
6. Verify B can retrieve its marker but cannot retrieve or directly read A's IDs.
7. Test strict session scope and subject filtering.
8. Delete `memory-02`; verify the API excludes it.
9. Correct `memory-01`. Wait for its new version's vector visibility and verify the API returns the new version and excludes the old one.

API deletion/correction checks include logical validity/version filtering. Dataset A separately verifies physical SQL UPDATE/DELETE maintenance. Retain or clean up only explicitly identified synthetic test data.

## Benchmark boundaries

Twenty rows establish basic correctness only. A performance study needs fixed vectors/query sets, larger datasets, warm-up, repeated samples, and comparable Recall@K. Separate SQL latency from embedding/API latency. The smoke index's capacity 128 is not a production sizing model.
