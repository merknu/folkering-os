-- Add content hashing for change detection
-- Phase 1.5 Day 3

-- ============================================================================
-- ADD CONTENT_HASH COLUMN TO FILE_PATHS
-- ============================================================================
-- Store SHA-256 hash of file contents to detect changes
-- Even if mtime changes, skip re-indexing if hash matches

ALTER TABLE file_paths ADD COLUMN content_hash TEXT;

-- Index for fast hash lookups
CREATE INDEX IF NOT EXISTS idx_file_paths_hash ON file_paths(content_hash);

-- ============================================================================
-- ADD LAST_INDEXED TIMESTAMP
-- ============================================================================
-- Track when file was last indexed (for debugging and maintenance)

ALTER TABLE file_paths ADD COLUMN last_indexed TEXT;

-- ============================================================================
-- USAGE
-- ============================================================================
-- Before re-indexing a file:
-- 1. Compute current hash
-- 2. Compare with stored hash
-- 3. If same, skip indexing (optimization)
-- 4. If different, re-index and update hash

-- Example query:
-- SELECT path, content_hash, last_indexed
-- FROM file_paths
-- WHERE content_hash IS NOT NULL
-- ORDER BY last_indexed DESC;
