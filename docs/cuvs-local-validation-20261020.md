# Historical CPU-only validation: 2026-10-20

This is a historical record for the original experiment based on upstream commit `0695ad61eec9104478f05f5824b75bdbeeca5616`. It is not validation of the current review head. Current design, access, datasets, and GPU evidence are in the [review package](experiments/cuvs-cagra/README.md).

## Environment and isolation

- ARM64 host, Rust/Cargo 1.96.0.
- A newly created disposable local Docker CPU MatrixOne 4.2.5, limited to 2 CPU and 2 GiB RAM, with SQL exposed only on loopback.
- Synthetic memories and a fixed four-dimensional mock embedder; no real LLM/embedding calls.
- Inherited remote database/LLM/embedding configuration removed from test processes.
- The opt-in integration fixture uses its own environment variable and literal loopback host guard, not generic DATABASE_URL.
- The local container and temporary storage were removed after testing.

## Recorded checks

Commands ran from the nested `memoria/` workspace.

| Check | Historical result |
| --- | --- |
| `cargo check --locked --workspace --tests` | Passed |
| `cargo test --locked -p memoria-service --lib` | 71 passed |
| `cargo test --locked -p memoria-storage --lib` | 68 passed; one existing database branch probe ignored |
| `cargo test --locked -p memoria-cli --bin memoria` | 28 passed |
| `cargo test --locked -p memoria-service --test cuvs_local_it -- --ignored --test-threads=1` | 2 passed on the disposable local CPU database |
| New Rust-file formatting and `git diff --check` | Passed |

Total: 169 passing tests. Full external-service/database suites were not run.

## Coverage and limits

Coverage included internal integer keys, stable string IDs, idempotent writes, user isolation, cross-session and strict-session retrieval, graph-path exclusion, vector modification, soft/physical deletion, policy propagation to background stores, and rejection of legacy memory tables without key conversion.

The CPU test did not establish CUDA/cuVS availability, CAGRA construction/search/CDC, GPU memory use, actual HTTP integration with real embeddings, or performance benefits. The later GPU run is recorded separately in [validation.md](experiments/cuvs-cagra/validation.md). Advanced branch/snapshot/merge/history flows remain outside initial acceptance.
