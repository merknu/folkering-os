# Synapse - Revised Implementation Plan
**Based on Technical Specification Analysis**

**Date:** 2026-01-25
**Status:** Revising from Phase 1 → Spec-Compliant Implementation

---

## Executive Summary

Current Synapse implementation (Phase 1) provides a working graph filesystem but has **10 critical gaps** compared to the technical specification. This document outlines a revised plan to achieve full spec compliance while maintaining the working features.

**Key Philosophy:** Local-First, No Cloud Dependency, File System as Truth

---

## Architecture Revision

### Current Architecture (Phase 1)
```
File System → Observer → SQLite → Query Engine → CLI
     ↓            ↓          ↓
  Mutable    Immediate   Basic      Basic        Text
             Events      Graph      Queries      Interface
```

### Target Architecture (Spec-Compliant)
```
File System → Debounced Observer → Multi-Process Indexer
     ↓              ↓                      ↓
  Mutable      Filtered Events      Neural Engine (ONNX)
  (Truth)      (Atomic writes)      ├─ GLiNER (NER)
                                    └─ SentenceTransformers
                                           ↓
                                    SQLite + sqlite-vec
                                    ├─ Resources (files)
                                    ├─ Entities (people/orgs)
                                    ├─ Relationships (graph)
                                    └─ Embeddings (vectors)
                                           ↓
                                    Query Engine (Recursive CTEs)
                                           ↓
                                    Visualization (Sigma.js)
                                           ↓
                                    Desktop UI (Qt/Tauri)
```

---

## Revised Database Schema

### Schema 2.0 (Spec-Compliant)

```sql
-- Project metadata (NEW)
CREATE TABLE project_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
INSERT INTO project_meta VALUES ('root_path', '/home/user/projects/myproject');

-- Resources (files) - RENAMED FROM nodes
CREATE TABLE resources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT UNIQUE NOT NULL,          -- Relative to project root
    filename TEXT NOT NULL,
    file_type TEXT,
    content_hash TEXT,                  -- SHA-256 (NEW)
    last_indexed INTEGER,               -- Unix timestamp (NEW)
    created_at TEXT,
    updated_at TEXT
);

-- Entities (extracted concepts) - NEW TABLE
CREATE TABLE entities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    label TEXT NOT NULL,                -- 'PERSON', 'ORG', 'LOC', 'DATE'
    canonical_name TEXT GENERATED ALWAYS AS (lower(name)) STORED,
    confidence REAL DEFAULT 1.0,        -- From GLiNER
    UNIQUE(canonical_name, label)
);

-- Relationships (polymorphic edges) - ENHANCED
CREATE TABLE relationships (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    source_type TEXT NOT NULL CHECK(source_type IN ('resource', 'entity')),
    target_id INTEGER NOT NULL,
    target_type TEXT NOT NULL CHECK(target_type IN ('resource', 'entity')),
    predicate TEXT NOT NULL,            -- 'mentions', 'contains', 'edited_by'
    weight REAL DEFAULT 1.0,
    confidence REAL DEFAULT 1.0,        -- From NER confidence
    created_at TEXT,
    UNIQUE(source_id, source_type, target_id, target_type, predicate)
);

-- Vector embeddings (local storage) - NEW
CREATE VIRTUAL TABLE vec_items USING vec0(
    embedding float[384]
);

CREATE TABLE embedding_map (
    rowid INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL,
    item_type TEXT NOT NULL CHECK(item_type IN ('resource', 'entity')),
    FOREIGN KEY(rowid) REFERENCES vec_items(rowid) ON DELETE CASCADE
);

-- Sessions (persist to database) - ENHANCED
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    is_active INTEGER DEFAULT 1
);

-- Session events (actually insert rows) - ENHANCED
CREATE TABLE session_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    resource_id INTEGER NOT NULL,      -- Changed from file_id
    event_type TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id),
    FOREIGN KEY(resource_id) REFERENCES resources(id)
);
```

---

## Implementation Phases (Revised)

### Phase 1.5: Critical Fixes (Week 3) - 4 days

**Goal:** Make current implementation spec-compliant for core storage

#### Task 1.1: Relative Path Resolution (1 day)
**Problem:** Absolute paths break when database is moved

**Implementation:**
```rust
// src/graph/mod.rs
pub struct GraphDB {
    db: SqlitePool,
    project_root: PathBuf,  // NEW
}

impl GraphDB {
    pub async fn new(db: SqlitePool) -> Result<Self> {
        // Read or set project root
        let root = Self::get_project_root(&db).await?
            .unwrap_or_else(|| std::env::current_dir().unwrap());

        Ok(Self { db, project_root: root })
    }

    async fn get_project_root(db: &SqlitePool) -> Result<Option<PathBuf>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM project_meta WHERE key = 'root_path'"
        ).fetch_optional(db).await?;

        Ok(row.map(|(path,)| PathBuf::from(path)))
    }

    pub async fn register_path(&self, node_id: &str, absolute_path: &str) -> Result<()> {
        // Convert to relative
        let abs = PathBuf::from(absolute_path);
        let relative = abs.strip_prefix(&self.project_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| absolute_path.to_string());

        sqlx::query(
            "INSERT INTO file_paths (node_id, path) VALUES (?, ?)
             ON CONFLICT(node_id) DO UPDATE SET path = excluded.path"
        )
        .bind(node_id)
        .bind(&relative)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    pub async fn resolve_path(&self, node_id: &str) -> Result<PathBuf> {
        let relative: String = sqlx::query_scalar(
            "SELECT path FROM file_paths WHERE node_id = ?"
        )
        .bind(node_id)
        .fetch_one(&self.db)
        .await?;

        Ok(self.project_root.join(relative))
    }
}
```

**Test:**
```rust
#[tokio::test]
async fn test_path_portability() {
    let db = create_test_db().await;
    let graph = GraphDB::new(db).await.unwrap();

    // Register with absolute path
    graph.register_path("123", "/home/user/project/file.txt").await.unwrap();

    // Verify stored as relative
    let stored: String = sqlx::query_scalar("SELECT path FROM file_paths WHERE node_id = '123'")
        .fetch_one(&graph.db).await.unwrap();
    assert_eq!(stored, "file.txt");

    // Verify resolves correctly
    let resolved = graph.resolve_path("123").await.unwrap();
    assert!(resolved.ends_with("file.txt"));
}
```

#### Task 1.2: Debounced Observer (1 day)
**Problem:** Atomic writes trigger multiple events, temp files get indexed

**Implementation:**
```rust
// src/observer/debouncer.rs
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;

const DEBOUNCE_INTERVAL: Duration = Duration::from_secs(1);
const IGNORE_EXTENSIONS: &[&str] = &[".swp", ".tmp", "~", ".git"];
const IGNORE_DIRS: &[&str] = &[".git", "__pycache__", "node_modules", ".idea"];

pub struct DebouncedObserver {
    pending: Arc<Mutex<HashMap<PathBuf, (Instant, String)>>>,  // path -> (last_event_time, event_type)
    db: Option<SqlitePool>,
}

impl DebouncedObserver {
    pub fn new(db: SqlitePool) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            db: Some(db),
        }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        // Check extensions
        if let Some(ext) = path.extension() {
            if IGNORE_EXTENSIONS.contains(&ext.to_str().unwrap_or("")) {
                return true;
            }
        }

        // Check directory components
        for component in path.components() {
            if let Some(name) = component.as_os_str().to_str() {
                if IGNORE_DIRS.contains(&name) || name.starts_with('.') {
                    return true;
                }
            }
        }

        false
    }

    pub async fn handle_event(&self, path: PathBuf, event_type: String) {
        if self.is_ignored(&path) {
            return;
        }

        // Update pending map
        {
            let mut pending = self.pending.lock().unwrap();
            pending.insert(path.clone(), (Instant::now(), event_type.clone()));
        }

        // Spawn debounce timer
        let pending = self.pending.clone();
        let db = self.db.clone();
        tokio::spawn(async move {
            sleep(DEBOUNCE_INTERVAL).await;

            // Check if event is still pending
            let should_process = {
                let mut pending_lock = pending.lock().unwrap();
                if let Some((last_time, _)) = pending_lock.get(&path) {
                    // If more than DEBOUNCE_INTERVAL has passed, process it
                    if last_time.elapsed() >= DEBOUNCE_INTERVAL {
                        pending_lock.remove(&path);
                        true
                    } else {
                        false  // Another event came in, wait more
                    }
                } else {
                    false  // Already processed
                }
            };

            if should_process && path.exists() {
                println!("[DEBOUNCED] Processing: {:?} ({})", path, event_type);
                // Process the file
                if let Some(db) = db {
                    Self::process_file(db, path, event_type).await;
                }
            }
        });
    }

    async fn process_file(db: SqlitePool, path: PathBuf, event_type: String) {
        // This is where we compute hash, extract text, run NER, etc.
        match event_type.as_str() {
            "created" | "modified" => {
                // TODO: Compute hash
                // TODO: Extract entities
                // TODO: Create embeddings
                println!("Would index: {:?}", path);
            }
            "deleted" => {
                // TODO: Mark resource as deleted
                println!("Would delete: {:?}", path);
            }
            _ => {}
        }
    }
}
```

**Test:**
```rust
#[tokio::test]
async fn test_atomic_write_debouncing() {
    let observer = DebouncedObserver::new(db);

    // Simulate VSCode atomic save
    observer.handle_event(PathBuf::from("file.txt.swp"), "created".into()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    observer.handle_event(PathBuf::from("file.txt.swp"), "modified".into()).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    observer.handle_event(PathBuf::from("file.txt"), "moved".into()).await;

    // Wait for debounce
    tokio::time::sleep(Duration::from_millis(1200)).await;

    // Verify only final file was indexed (not .swp)
    // (Check database for single entry)
}
```

#### Task 1.3: Content Hashing (0.5 day)
**Problem:** Re-indexes unchanged files

**Implementation:**
```rust
use sha2::{Sha256, Digest};
use std::fs::File;
use std::io::Read;

pub fn compute_file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];

    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

// In process_file:
async fn process_file(db: SqlitePool, path: PathBuf, event_type: String) {
    let new_hash = compute_file_hash(&path).unwrap();

    // Check if hash changed
    let old_hash: Option<String> = sqlx::query_scalar(
        "SELECT content_hash FROM resources WHERE path = ?"
    )
    .bind(path.to_str())
    .fetch_optional(&db)
    .await
    .ok()
    .flatten();

    if old_hash.as_ref() == Some(&new_hash) {
        println!("File unchanged, skipping indexing");
        return;
    }

    println!("Hash changed, re-indexing...");
    // Continue with indexing...
}
```

#### Task 1.4: Session Persistence (0.5 day)
**Problem:** Session events not written to database

**Implementation:**
```rust
// In Observer::handle_file_access_with_id
pub async fn handle_file_access_with_id(&self, file_id: String) {
    let mut session_lock = self.current_session.lock().await;

    if session_lock.is_none() || !session_lock.as_ref().unwrap().is_active() {
        let new_session = FileAccessSession::new(None);

        // PERSIST session to database
        if let Some(db) = &self.db {
            sqlx::query(
                "INSERT INTO sessions (id, started_at, is_active) VALUES (?, datetime('now'), 1)"
            )
            .bind(&new_session.session_id)
            .execute(db)
            .await
            .ok();
        }

        *session_lock = Some(new_session);
    }

    let session = session_lock.as_mut().unwrap();
    session.add_file(file_id.clone());

    // PERSIST event to database
    if let Some(db) = &self.db {
        sqlx::query(
            "INSERT INTO session_events (session_id, file_id, event_type, timestamp)
             VALUES (?, ?, 'open', datetime('now'))"
        )
        .bind(&session.session_id)
        .bind(&file_id)
        .execute(db)
        .await
        .ok();
    }

    // ... rest of logic
}
```

---

### Phase 2: Neural Intelligence (Week 4-5) - 4 days

#### Task 2.1: Polymorphic Schema Migration (1 day)
**Goal:** Support resource↔entity relationships

**Migration Script:**
```sql
-- Rename nodes to resources
ALTER TABLE nodes RENAME TO resources;

-- Create entities table
CREATE TABLE entities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    label TEXT NOT NULL,
    canonical_name TEXT GENERATED ALWAYS AS (lower(name)) STORED,
    confidence REAL DEFAULT 1.0,
    UNIQUE(canonical_name, label)
);

-- Migrate edges to relationships with types
CREATE TABLE relationships_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    source_type TEXT NOT NULL DEFAULT 'resource',
    target_id INTEGER NOT NULL,
    target_type TEXT NOT NULL DEFAULT 'resource',
    predicate TEXT NOT NULL,
    weight REAL DEFAULT 1.0,
    confidence REAL DEFAULT 1.0,
    created_at TEXT
);

INSERT INTO relationships_new (source_id, target_id, predicate, weight, created_at)
SELECT
    CAST(source_id AS INTEGER),
    CAST(target_id AS INTEGER),
    type,
    weight,
    created_at
FROM edges;

DROP TABLE edges;
ALTER TABLE relationships_new RENAME TO relationships;
```

#### Task 2.2: ONNX Integration (2 days)
**Challenge:** No mature Rust ONNX bindings

**Solution:** Python subprocess

```rust
// src/neural/mod.rs
use std::process::{Command, Stdio};
use serde_json::json;

pub struct GLiNERService {
    // Python process handle
}

impl GLiNERService {
    pub fn new() -> Result<Self> {
        // Start Python subprocess
        // python neural_server.py
        Ok(Self {})
    }

    pub async fn extract_entities(&self, text: &str) -> Result<Vec<Entity>> {
        // Send JSON-RPC request to Python subprocess
        let request = json!({
            "text": text,
            "labels": ["PERSON", "ORG", "LOC", "DATE"]
        });

        // Get response
        // Parse entities with confidence scores
        Ok(vec![])
    }
}
```

**Python Neural Server:**
```python
# neural_server.py
import sys
import json
import onnxruntime as ort
from tokenizers import Tokenizer

class GLiNERService:
    def __init__(self, model_path="gliner.onnx"):
        self.session = ort.InferenceSession(model_path)
        self.tokenizer = Tokenizer.from_file("tokenizer.json")

    def extract(self, text, labels):
        # Tokenize
        encoding = self.tokenizer.encode(text)

        # Run inference
        outputs = self.session.run(None, {...})

        # Decode entities
        entities = []
        for span, label, score in decode(outputs):
            entities.append({
                "text": text[span[0]:span[1]],
                "label": label,
                "confidence": float(score)
            })
        return entities

# JSON-RPC loop
service = GLiNERService()
for line in sys.stdin:
    request = json.loads(line)
    result = service.extract(request['text'], request['labels'])
    print(json.dumps(result), flush=True)
```

#### Task 2.3: sqlite-vec Integration (1 day)
**Challenge:** Requires C compilation

**Solution:** Use pre-compiled extension

```rust
use rusqlite::{Connection, LoadExtensionGuard};

pub fn load_sqlite_vec(conn: &Connection) -> Result<()> {
    unsafe {
        let _guard = LoadExtensionGuard::new(conn)?;
        conn.load_extension("vec0", None)?;
    }
    Ok(())
}

pub async fn insert_embedding(db: &SqlitePool, item_id: i64, item_type: &str, vector: &[f32]) -> Result<()> {
    // Serialize vector to bytes
    let blob = vector.iter()
        .flat_map(|f| f.to_le_bytes())
        .collect::<Vec<u8>>();

    // Insert into virtual table
    sqlx::query("INSERT INTO vec_items(embedding) VALUES (?)")
        .bind(&blob)
        .execute(db)
        .await?;

    let rowid = /* get last rowid */;

    // Map to item
    sqlx::query("INSERT INTO embedding_map(rowid, item_id, item_type) VALUES (?, ?, ?)")
        .bind(rowid)
        .bind(item_id)
        .bind(item_type)
        .execute(db)
        .await?;

    Ok(())
}
```

---

### Phase 3: Visualization (Week 6-8) - 5 days

#### Approach: Tauri (Rust-native)

**Why Tauri over PyQt6:**
- Pure Rust (no Python dependency)
- Modern web technologies (Svelte/React)
- Smaller binary size
- Better integration with existing code

**Architecture:**
```
Rust Backend (Tauri)
├─ GraphDB (existing)
├─ QueryEngine (existing)
└─ Tauri Commands (RPC)
       ↓
Web Frontend (Svelte + Sigma.js)
├─ Graph Visualization
├─ Search Interface
└─ Node Detail View
```

**Implementation:**
```rust
// src-tauri/main.rs
#[tauri::command]
async fn get_graph_neighborhood(node_id: String, hops: i32) -> Result<GraphData, String> {
    let db = get_db().await;
    let query = QueryEngine::new(db);
    let (nodes, edges) = query.get_neighborhood(&node_id, hops).await
        .map_err(|e| e.to_string())?;

    Ok(GraphData { nodes, edges })
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![get_graph_neighborhood])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

**Frontend (Svelte + Sigma.js):**
```svelte
<script>
import { onMount } from 'svelte';
import Sigma from 'sigma';
import Graph from 'graphology';
import { invoke } from '@tauri-apps/api/tauri';

let sigmaInstance;

onMount(async () => {
    const data = await invoke('get_graph_neighborhood', { nodeId: 'root', hops: 2 });

    const graph = new Graph();
    data.nodes.forEach(n => graph.addNode(n.id, n.attributes));
    data.edges.forEach(e => graph.addEdge(e.source, e.target));

    sigmaInstance = new Sigma(graph, document.getElementById('container'));
});
</script>

<div id="container" style="width: 100vw; height: 100vh;"></div>
```

---

## Timeline Summary

| Phase | Duration | Key Deliverables |
|-------|----------|------------------|
| 1.5 - Critical Fixes | 4 days | Relative paths, debouncing, hashing, persistence |
| 2 - Neural | 4 days | ONNX NER, embeddings, polymorphic schema |
| 3 - Visualization | 5 days | Tauri app, Sigma.js renderer |
| **Total** | **13 days** | **Full spec compliance** |

---

## Success Criteria

**Phase 1.5:**
- [ ] Database portable across machines
- [ ] Atomic saves handled correctly
- [ ] Unchanged files not re-indexed
- [ ] Session events queryable

**Phase 2:**
- [ ] Entity extraction from text
- [ ] Vector similarity search working
- [ ] Confidence scores on edges

**Phase 3:**
- [ ] Interactive graph visualization
- [ ] Click node → expand neighborhood
- [ ] Search finds nodes/entities

---

## Next Steps

1. Review IMPLEMENTATION_GAPS.md
2. Start Phase 1.5 Task 1.1 (Relative Paths)
3. Run tests after each task
4. Document findings in CHANGELOG.md
5. Move to Phase 2 only after 1.5 complete

**Do NOT add new features until critical fixes done.**
