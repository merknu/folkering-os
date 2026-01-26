-- Migration 007: Vector Search Tables
-- Phase 2 Day 4: sqlite-vec integration

-- Virtual table for vector embeddings (requires sqlite-vec extension)
-- Note: This uses vec0 virtual table module from sqlite-vec
-- The actual table creation will be done when sqlite-vec is loaded
-- For now, this is documentation of the schema

-- Virtual table schema (created when extension loaded):
-- CREATE VIRTUAL TABLE vec_nodes USING vec0(
--   embedding float[384]
-- );
--
-- The vec0 virtual table provides:
-- - rowid (INTEGER PRIMARY KEY) - auto-generated
-- - embedding (FLOAT[384]) - vector data
-- - MATCH operator for k-NN search
-- - distance metric (cosine by default)

-- Mapping table: nodes.id → vec_nodes.rowid
CREATE TABLE IF NOT EXISTS node_embeddings (
    node_id TEXT PRIMARY KEY,
    vec_rowid INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

-- Index for reverse lookup (vec_rowid → node_id)
CREATE INDEX IF NOT EXISTS idx_node_embeddings_vec ON node_embeddings(vec_rowid);

-- For databases WITHOUT sqlite-vec extension, create a fallback table
-- This allows tests to run without the extension
CREATE TABLE IF NOT EXISTS vec_nodes (
    rowid INTEGER PRIMARY KEY AUTOINCREMENT,
    embedding TEXT NOT NULL  -- JSON array of floats
);

-- Note: When sqlite-vec extension is available, the virtual table
-- will shadow this regular table. The virtual table provides:
-- - SIMD-optimized vector operations
-- - Fast k-NN search
-- - Multiple distance metrics
--
-- Usage example:
--   SELECT rowid, distance
--   FROM vec_nodes
--   WHERE embedding MATCH '[0.1, 0.2, ..., 0.384]'
--     AND k = 10
--   ORDER BY distance;
