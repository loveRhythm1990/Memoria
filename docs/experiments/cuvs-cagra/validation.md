# Validation evidence

## Source provenance

| Item | Commit |
| --- | --- |
| Memoria deployed for GPU acceptance | `0beab5a47aa045a35f6e9a9d76088ee45d27aadc` |
| Upstream main synchronized for this review | `e2aa9071f70b0f7a3b2f53d297ffa220015a3d0b` |
| GPU MatrixOne source in the acceptance run | `71498157cc21f833b8e717f45d50d1f926be3134` |

The feature was cherry-picked onto synchronized main. The GPU results below belong to the deployed old experimental commit, not to a new image built from this PR head. This PR does not update QA images or deployments.

## GPU acceptance: 2026-10-23

An isolated standalone MO Pod used one RTX 4090, 8 CPU, 32 GiB RAM, and a dedicated 200 GiB persistent data volume. Memoria used multi-db, BGE-M3/1024 dimensions, and graph retrieval disabled.

| Check | Result |
| --- | --- |
| SQL CAGRA build, 20 eight-dimensional vectors | Passed; about 10.99 s |
| SQL plan and five exact probes | Passed; `cagra_search` present |
| Post-index INSERT / UPDATE / DELETE visibility | Passed; about 5.23 / 9.27 / 10.29 s |
| Memoria write/embedding and internal BIGINT key | Passed; 20 memories, 20 vectors |
| CAGRA build on the user's memory table | Passed; about 8.52 s |
| API cross-session retrieval before/after indexing | Passed |
| Vector branch hit / graph branch disabled | Passed |
| New memory direct-vector visibility / API recall | Passed; vector visibility about 8.37 s |
| User isolation and session/subject filters | Passed |
| Deleted memory excluded by API | Passed |
| Correction vector visibility / version filtering | Passed; about 10.46 s |
| Post-test service state | MO and Memoria Ready, zero restarts |

The two reports contain 23 passing functional assertions. Visibility measurements are single-run observations with approximately one-second polling, not latency guarantees. All test inputs were synthetic and are included in this directory.

## Actual API plan evidence is still missing

The direct SQL test and a source-equivalent Memoria query produced CAGRA plans. API explain information confirmed vector candidates and graph retrieval disabled. Statement history/process-list sampling did not yield the API's actual full vector SQL.

The API report retains `actual_query_capture_gap=true`. A source-equivalent `EXPLAIN` must not be described as a captured API execution plan. Stronger runtime plan evidence is still needed for strict verification of individual API queries.

## No performance conclusion

One HTTP sample was about 158 ms before indexing and 257 ms afterward. The tiny dataset and end-to-end request include embedding, application work, SQL, and network transport. These two readings establish neither a speedup nor a slowdown.

Large-data Recall@K, latency percentiles, throughput, capacity limits, and restart/recovery remain unmeasured. Branch/snapshot restore/merge and historical table flows also remain outside initial acceptance.

## Synchronized-branch code validation

Fresh-branch checks passed on 2026-10-23:

| Command / fixture | Result |
| --- | --- |
| `cargo check --locked --workspace --tests` | Passed |
| `cargo test --locked -p memoria-storage -p memoria-service --lib -- --test-threads=1` | 139 passed; one existing database probe ignored |
| `cargo test --locked -p memoria-cli --bin memoria -- --test-threads=1` | 28 passed |
| `cargo test --locked -p memoria-service --test cuvs_local_it -- --ignored --test-threads=1` | 2 passed on a newly created loopback-only CPU MO 4.2.5 |

Total: 169 passed, plus the one existing ignored probe. Test processes did not inherit remote database/LLM/embedding settings. The temporary local Docker database was removed after the integration run.

Historical CPU-only results at the old base are in [the original local-validation record](../../cuvs-local-validation-20261020.md). Neither local run is a GPU deployment of the synchronized PR head.
