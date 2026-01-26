-- Migration 008: Full-Text Search (FTS5)
-- Phase 2 Day 6: Hybrid search preparation

-- FTS5 virtual table for full-text search on file content
-- Note: content_rowid links to nodes.rowid (not nodes.id which is TEXT)
-- We'll need a workaround using a mapping table

-- Content table to store file text content
CREATE TABLE IF NOT EXISTS file_content (
    node_id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

-- FTS5 virtual table for full-text search
-- Uses external content table pattern for efficiency
CREATE VIRTUAL TABLE IF NOT EXISTS file_content_fts USING fts5(
    content,
    content_rowid=rowid,
    tokenize='porter unicode61'
);

-- Triggers to keep FTS5 in sync with file_content table
-- Note: FTS5 doesn't support triggers directly on virtual tables
-- So we trigger on the content table and update FTS5

-- Trigger: Insert
CREATE TRIGGER IF NOT EXISTS file_content_ai AFTER INSERT ON file_content BEGIN
    INSERT INTO file_content_fts(rowid, content)
    VALUES (NEW.rowid, NEW.content);
END;

-- Trigger: Update
CREATE TRIGGER IF NOT EXISTS file_content_au AFTER UPDATE ON file_content BEGIN
    UPDATE file_content_fts
    SET content = NEW.content
    WHERE rowid = NEW.rowid;
END;

-- Trigger: Delete
CREATE TRIGGER IF NOT EXISTS file_content_ad AFTER DELETE ON file_content BEGIN
    DELETE FROM file_content_fts WHERE rowid = OLD.rowid;
END;

-- Index for faster lookups
CREATE INDEX IF NOT EXISTS idx_file_content_node ON file_content(node_id);

-- Example queries:
--
-- Simple keyword search:
--   SELECT node_id, rank
--   FROM file_content fc
--   JOIN file_content_fts fts ON fts.rowid = fc.rowid
--   WHERE file_content_fts MATCH 'machine learning'
--   ORDER BY rank;
--
-- Search with BM25 ranking:
--   SELECT node_id, bm25(file_content_fts) as rank
--   FROM file_content fc
--   JOIN file_content_fts fts ON fts.rowid = fc.rowid
--   WHERE file_content_fts MATCH 'machine learning'
--   ORDER BY rank DESC;
--
-- Phrase search:
--   WHERE file_content_fts MATCH '"neural networks"'
--
-- Boolean search:
--   WHERE file_content_fts MATCH 'machine AND learning NOT deep'
