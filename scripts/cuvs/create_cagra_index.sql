-- Run explicitly in the dedicated TEST USER database, not the shared registry.
-- Requires a GPU MatrixOne build/runtime that has passed standalone SQL checks.
-- This script is not run by Memoria startup or by the CPU local tests.
-- Inspect SHOW CREATE TABLE first; do not rerun blindly when the index exists.
-- Small graph parameters below are for flow validation only, not performance tuning.
-- Import at least 10 valid, fixed-dimension vectors before this small smoke test.

SET experimental_cagra_index = 1;
SET cagra_threads_build = 2;
SET cagra_max_index_capacity = 128;

CREATE INDEX mem_memories_embedding_cagra USING cagra
ON mem_memories (embedding)
op_type 'vector_l2_ops'
intermediate_graph_degree=16 graph_degree=8 itopk_size=32;

SHOW CREATE TABLE mem_memories;

-- Next: EXPLAIN the actual Memoria vector SQL with its real filters and dimension.
-- An API result or vector_hit=true alone is not proof of CAGRA usage.
