# cuVS / CAGRA experiment review package

This opt-in branch prepares Memoria for an isolated GPU MatrixOne experiment. It is not a production rollout proposal.

Read these documents in order:

1. [Design](design.md): internal integer keys, stable API IDs, policy propagation, graph/IVF isolation, and scope.
2. [Service access](access.md): generic Kubernetes access and authenticated API examples. Live environment details and credentials are intentionally omitted.
3. [Datasets and reproduction](datasets.md): all synthetic inputs and expected results.
4. [Validation](validation.md): measured evidence, source provenance, and remaining gaps.

## Configuration

```text
MEMORIA_CUVS_EXPERIMENT=true
MEMORIA_MULTI_DB=true
MEMORIA_GRAPH_RETRIEVAL_ENABLED=false
ENTITY_POOL_SIZE=0
```

Provide database URLs, credentials, and embedding settings through the deployment's existing secret mechanism. The acceptance deployment used `BAAI/bge-m3`, 1024 dimensions, through a separate embedding service. MatrixOne's GPU runs vector indexing/search, not the embedding model.

Use a fresh isolated shared registry and fresh user databases. Existing incompatible memory tables are rejected, not converted. Without the experiment flag, the current memory-key schema and IVF behavior remain the default.

Memoria does not automatically create CAGRA indexes. After importing memories, use [the explicit SQL example](../../../scripts/cuvs/create_cagra_index.sql) in each target user's database. Creating an index in the shared registry does not index all users.

## Code checks

From the repository's `memoria/` directory, without inherited remote database/LLM/embedding configuration:

```bash
cargo check --locked --workspace --tests
cargo test --locked -p memoria-storage -p memoria-service --lib -- --test-threads=1
cargo test --locked -p memoria-cli --bin memoria -- --test-threads=1
```

The separate `cuvs_local_it` integration target is ignored by default. It requires `MEMORIA_CUVS_LOCAL_DATABASE_URL`, accepts literal loopback hosts only, and must target a disposable local MatrixOne. It does not prove GPU correctness. Run it explicitly with `--ignored --test-threads=1` only after preparing that fixture.
