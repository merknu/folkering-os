-- Synapse Graph Filesystem Schema
-- Version: 0.1.0
-- Philosophy: Files exist in a web of context, not just locations

-- ============================================================================
-- NODES TABLE
-- ============================================================================
-- Represents any entity in the knowledge graph
CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY NOT NULL,  -- UUID
    type TEXT NOT NULL,             -- file, person, app, event, tag, project
    properties TEXT NOT NULL,       -- JSON blob
    created_at TEXT NOT NULL,       -- ISO 8601 timestamp
    updated_at TEXT NOT NULL,       -- ISO 8601 timestamp

    -- Index for fast type-based queries
    CHECK (type IN ('file', 'person', 'app', 'event', 'tag', 'project', 'location'))
);

CREATE INDEX idx_nodes_type ON nodes(type);
CREATE INDEX idx_nodes_created ON nodes(created_at);

-- ============================================================================
-- EDGES TABLE
-- ============================================================================
-- Represents relationships between nodes
CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id TEXT NOT NULL,        -- Origin node UUID
    target_id TEXT NOT NULL,        -- Destination node UUID
    type TEXT NOT NULL,             -- Relationship type
    weight REAL NOT NULL DEFAULT 0.5,  -- 0.0 to 1.0
    properties TEXT,                -- Optional JSON metadata
    created_at TEXT NOT NULL,       -- When this edge was discovered

    FOREIGN KEY (source_id) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY (target_id) REFERENCES nodes(id) ON DELETE CASCADE,

    -- Prevent duplicate edges (same source, target, type)
    UNIQUE(source_id, target_id, type),

    -- Valid relationship types
    CHECK (type IN (
        'CONTAINS',         -- Folder contains file
        'EDITED_BY',        -- File edited by person
        'OPENED_WITH',      -- File opened with app
        'MENTIONS',         -- File mentions person/entity
        'SHARED_WITH',      -- Shared between people
        'HAPPENED_DURING',  -- Event occurred during time window
        'CO_OCCURRED',      -- Files used together
        'SIMILAR_TO',       -- Semantic similarity
        'DEPENDS_ON',       -- Code dependency
        'REFERENCES',       -- Document references another
        'PARENT_OF',        -- Hierarchical relationship
        'TAGGED_WITH'       -- Has tag
    ))
);

CREATE INDEX idx_edges_source ON edges(source_id);
CREATE INDEX idx_edges_target ON edges(target_id);
CREATE INDEX idx_edges_type ON edges(type);
CREATE INDEX idx_edges_weight ON edges(weight DESC);
CREATE INDEX idx_edges_created ON edges(created_at);

-- Composite index for fast bidirectional lookups
CREATE INDEX idx_edges_both ON edges(source_id, target_id);

-- ============================================================================
-- QUERIES (SAVED QUERIES)
-- ============================================================================
-- Store common graph traversal patterns
CREATE TABLE IF NOT EXISTS saved_queries (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    description TEXT,
    query_pattern TEXT NOT NULL,  -- SQL CTE template
    parameters TEXT,               -- JSON schema for params
    created_at TEXT NOT NULL
);

-- ============================================================================
-- SESSIONS (WORKING CONTEXT)
-- ============================================================================
-- Track file access sessions for temporal analysis
CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT,                  -- Person node ID
    started_at TEXT NOT NULL,
    ended_at TEXT,
    -- Session is "active" for 5 minutes after last activity
    is_active INTEGER DEFAULT 1,

    CHECK (is_active IN (0, 1))
);

CREATE INDEX idx_sessions_user ON sessions(user_id);
CREATE INDEX idx_sessions_active ON sessions(is_active, started_at);

-- ============================================================================
-- SESSION_EVENTS (FILE ACCESSES)
-- ============================================================================
-- Log every file access for building CO_OCCURRED edges
CREATE TABLE IF NOT EXISTS session_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    file_id TEXT NOT NULL,         -- Node ID of file
    event_type TEXT NOT NULL,      -- open, edit, close, save
    timestamp TEXT NOT NULL,

    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (file_id) REFERENCES nodes(id) ON DELETE CASCADE,

    CHECK (event_type IN ('open', 'edit', 'close', 'save'))
);

CREATE INDEX idx_events_session ON session_events(session_id);
CREATE INDEX idx_events_file ON session_events(file_id);
CREATE INDEX idx_events_timestamp ON session_events(timestamp);

-- ============================================================================
-- VECTOR_EMBEDDINGS (PHASE 2)
-- ============================================================================
-- Store vector embeddings for semantic search
-- Note: SQLite doesn't have native vector support, so we'll use an external
-- vector DB (Qdrant/Milvus) and just store the reference here
CREATE TABLE IF NOT EXISTS vector_embeddings (
    node_id TEXT PRIMARY KEY NOT NULL,
    vector_id TEXT NOT NULL,       -- ID in external vector DB
    model TEXT NOT NULL,           -- e.g., "all-MiniLM-L6-v2"
    embedding_dim INTEGER NOT NULL, -- Vector dimension
    created_at TEXT NOT NULL,

    FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

-- ============================================================================
-- MATERIALIZED PATHS (LEGACY COMPATIBILITY)
-- ============================================================================
-- Maintain traditional path mapping for backward compatibility
CREATE TABLE IF NOT EXISTS file_paths (
    node_id TEXT PRIMARY KEY NOT NULL,
    path TEXT NOT NULL UNIQUE,     -- Traditional filesystem path

    FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE INDEX idx_paths_path ON file_paths(path);

-- ============================================================================
-- INITIAL DATA
-- ============================================================================

-- Create root "system" nodes
INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES
    ('00000000-0000-0000-0000-000000000001', 'tag',
     '{"name": "work", "color": "#3b82f6"}',
     datetime('now'), datetime('now')),
    ('00000000-0000-0000-0000-000000000002', 'tag',
     '{"name": "personal", "color": "#10b981"}',
     datetime('now'), datetime('now')),
    ('00000000-0000-0000-0000-000000000003', 'tag',
     '{"name": "important", "color": "#ef4444"}',
     datetime('now'), datetime('now'));

-- Example saved query: "Files I worked on today"
INSERT INTO saved_queries (id, name, description, query_pattern, parameters, created_at) VALUES
    ('query_today',
     'Files worked on today',
     'Find all files accessed today',
     'SELECT DISTINCT n.* FROM nodes n
      JOIN session_events se ON n.id = se.file_id
      WHERE date(se.timestamp) = date("now")
      AND n.type = "file"
      ORDER BY se.timestamp DESC',
     '{}',
     datetime('now'));
