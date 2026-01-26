-- Project Metadata
-- Stores project-level configuration for database portability

-- ============================================================================
-- PROJECT_META TABLE
-- ============================================================================
-- Stores the project root path for relative path resolution
-- Only one row should exist in this table
CREATE TABLE IF NOT EXISTS project_meta (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Initialize with default project root (current working directory)
-- This will be overridden when GraphDB initializes
INSERT INTO project_meta (key, value, updated_at) VALUES
    ('project_root', '.', datetime('now'))
ON CONFLICT(key) DO NOTHING;

-- ============================================================================
-- UPDATE FILE_PATHS TABLE
-- ============================================================================
-- The file_paths table now stores RELATIVE paths instead of absolute paths
-- No schema changes needed, but the semantics change:
-- BEFORE: path = "C:\Users\merkn\project\src\main.rs"
-- AFTER:  path = "src/main.rs"  (relative to project_root)

-- Note: Path separators are always forward slashes (/) for cross-platform compatibility
-- Windows paths like "src\main.rs" are converted to "src/main.rs" on insert
