# cuVS / CAGRA experiment

The complete [review package](experiments/cuvs-cagra/README.md) contains the design, service access with placeholders, full synthetic datasets, reproduction steps, validation evidence, and remaining gaps.

This opt-in experiment changes only the internal main memory-table key to a signed BIGINT while retaining string business IDs. It disables graph retrieval and IVF management for the experimental instance. CAGRA is created explicitly per user database and maintained asynchronously by GPU MatrixOne.

Use fresh isolated databases. No automatic conversion of existing tables or implicit legacy migration is provided. Ordinary default-main-table memory operations are the initial scope; branch/snapshot restore/merge and historical table operations require separate validation.
