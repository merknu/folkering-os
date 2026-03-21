# Synapse Implementation Gap Analysis

**Date:** 2026-01-25
**Current Status:** Phase 1 Complete (Basic Graph)
**Target:** Full Technical Specification Compliance

---

## Critical Gaps Identified

### 1. Storage Model: Path Resolution ❌ MISSING

**Spec Requirement:**
> Paths are stored relative to the database root rather than absolute paths. This relative addressing is crucial for portability.

**Current Implementation:**
```rust
// file_paths table stores absolute paths
graph.register_path(&node.id, "/work/report.pdf").await?;
```

**Gap:** If database is moved to a new location, all paths break.

**Solution Required:**
- Store project root in metadata table
- Convert all paths to relative on insert
- Resolve to absolute when querying
- Add `project_root` field to database

---

### 2. Ingestion: Atomic Write Detection ❌ MISSING

**Spec Requirement:**
> Modern text editors employ an "atomic save" strategy: Create Temp → Flush → Rename. A naive watcher listening for MODIFIED on file.txt might miss the event entirely.

**Current Implementation:**
```rust
// Basic event handling, no debouncing
match event.kind {
    EventKind::Access(_) => self.handle_file_access(path).await,
    EventKind::Modify(_) => self.handle_file_edit(path).await,
    _ => {}
}
```

**Gap:**
- No debouncing (events fire immediately)
- No temp file filtering (.swp, .tmp ignored)
- No atomic move detection
- No event coalescing

**Solution Required:**
- Implement debounced state machine (1.0s settling period)
- Filter IGNORE_EXTENSIONS: {'.swp', '.tmp', '.git', '~'}
- Filter IGNORE_DIRS: {'.git', '__pycache__', 'node_modules'}
- Detect FileMovedEvent as DELETE(src) + CREATE(dest)

---

### 3. Database Schema: Polymorphic Relationships ⚠️ INCOMPLETE

**Spec Requirement:**
```sql
CREATE TABLE relationships (
    source_id INTEGER NOT NULL,
    source_type TEXT CHECK(source_type IN ('resource', 'entity')),
    target_id INTEGER NOT NULL,
    target_type TEXT CHECK(target_type IN ('resource', 'entity')),
    predicate TEXT NOT NULL
);
```

**Current Implementation:**
```sql
CREATE TABLE edges (
    source_id TEXT NOT NULL,  -- UUID strings
    target_id TEXT NOT NULL,  -- UUID strings
    type TEXT NOT NULL        -- Edge type
);
```

**Gap:**
- No distinction between resources (files) and entities (people/orgs)
- Can't query "all files edited by person Alice" efficiently
- No confidence scores from NER

**Solution Required:**
- Add `source_type` and `target_type` columns
- Add `confidence` column (0.0-1.0 from GLiNER)
- Split nodes table into `resources` and `entities`
- Update all queries to check type

---

### 4. Entity Extraction: Neural Engine ❌ MISSING

**Spec Requirement:**
> GLiNER (Generalist and Lightweight Named Entity Recognition) via ONNX Runtime. Zero-shot extraction of arbitrary entity types.

**Current Implementation:**
```rust
// Simple regex patterns
fn extract_entities(&self, text: &str) -> Vec<String> {
    let email_regex = regex::Regex::new(r"\b[\w._%+-]+@[\w.-]+\.[A-Z]{2,}\b").unwrap();
    // ...
}
```

**Gap:**
- Only extracts emails and @mentions
- No proper Named Entity Recognition
- No Person/Organization/Location detection
- No confidence scores

**Solution Required:**
- Integrate ONNX Runtime (onnxruntime crate)
- Load GLiNER model (gliner.onnx + tokenizer)
- Implement zero-shot prediction:
  ```rust
  let entities = gliner.predict(text, &["PERSON", "ORG", "LOC"], 0.5)?;
  ```
- Store entities in separate table
- Create MENTIONS edges with confidence

---

### 5. Vector Embeddings: sqlite-vec ❌ NOT IMPLEMENTED

**Spec Requirement:**
```sql
CREATE VIRTUAL TABLE vec_items USING vec0(
    embedding float[384]
);
```

**Current Implementation:**
```sql
CREATE TABLE vector_embeddings (
    node_id TEXT PRIMARY KEY,
    vector_id TEXT,  -- External DB reference
    model TEXT
);
```

**Gap:**
- Embeddings stored in external vector DB (Qdrant planned)
- No local vector search
- Violates "local-first" principle

**Solution Required:**
- Install sqlite-vec extension
- Create virtual table for embeddings
- Implement embedding_map table
- Add vector similarity search:
  ```sql
  SELECT * FROM vec_items
  WHERE embedding MATCH ?
  ORDER BY distance LIMIT 10
  ```

---

### 6. Graph Traversal: Cycle Detection ⚠️ BASIC

**Spec Requirement:**
```sql
WITH RECURSIVE traversal(id, type, depth, path_trace) AS (
    ...
    WHERE t.path_trace NOT LIKE '%/' || new_id || '%'  -- Cycle prevention
)
```

**Current Implementation:**
```sql
WITH RECURSIVE neighborhood AS (
    SELECT id, 0 AS hop FROM nodes WHERE id = ?
    UNION ALL
    SELECT target_id, hop + 1 FROM edges
    WHERE hop < ?  -- Simple depth limit
)
```

**Gap:**
- No path tracing
- Can revisit nodes (infinite loops possible)
- No cycle detection

**Solution Required:**
- Add `path_trace` column to CTE
- Implement string concatenation for visited nodes
- Add LIKE filter to prevent revisits

---

### 7. Concurrency: Multi-Process Architecture ❌ MISSING

**Spec Requirement:**
> Indexer Process: A separate multiprocessing.Process is essential for the Neural Engine. Python's GIL would cause UI to stutter.

**Current Implementation:**
```rust
// Single-threaded observer
pub async fn start(&self, watch_path: PathBuf) -> Result<()> {
    // Blocks on file events
}
```

**Gap:**
- Observer runs in same thread as main
- No process separation for CPU-intensive work
- No async queue for batching

**Solution Required:**
- Separate indexer process/thread
- Use channels for event queue
- Process events in batches
- Non-blocking observer

---

### 8. Visualization: UI Layer ❌ COMPLETELY MISSING

**Spec Requirement:**
- PyQt6 desktop shell
- Sigma.js WebGL renderer
- QWebChannel for Python ↔ JavaScript bridge

**Current Implementation:**
- CLI only
- No graphical interface

**Gap:** Entire visualization layer missing

**Solution Required:**
- Create Qt6 window (or Tauri for Rust)
- Embed web view
- Implement Sigma.js graph renderer
- Bridge SQL queries to JavaScript

---

### 9. Content Hashing: Change Detection ⚠️ MISSING

**Spec Requirement:**
```sql
CREATE TABLE resources (
    content_hash TEXT,  -- SHA-256 for change detection
    ...
);
```

**Current Implementation:**
```rust
// No content hashing
pub struct FileProperties {
    pub name: String,
    pub size: u64,
    pub content_hash: Option<String>,  // Always None
}
```

**Gap:**
- Can't detect if file content changed
- Re-indexes files unnecessarily
- No deduplication

**Solution Required:**
- Compute SHA-256 on file read
- Store in properties
- Skip indexing if hash unchanged

---

### 10. Session Tracking: Time Windows ⚠️ INCOMPLETE

**Spec Requirement:**
> 5-minute session windows. Files accessed within same session create CO_OCCURRED edges.

**Current Implementation:**
```rust
pub fn is_active(&self) -> bool {
    let elapsed = now.signed_duration_since(self.last_activity);
    elapsed < Duration::minutes(5)
}
```

**Gap:**
- Session tracking works
- But sessions aren't persisted to database
- No session_events table entries
- Can't query "what did I work on yesterday"

**Solution Required:**
- Actually insert into session_events table
- Link to sessions table
- Implement temporal queries

---

## Prioritized Implementation Plan

### Phase 1.5 (Week 3) - Critical Fixes

**Priority: HIGH** (Spec compliance, no new features)

1. **Relative Path Resolution** (1 day)
   - Add `project_root` to metadata
   - Convert all paths to relative
   - Update all queries

2. **Debounced File Watcher** (1 day)
   - Port debouncing logic from spec
   - Add temp file filtering
   - Handle atomic moves

3. **Content Hashing** (0.5 day)
   - Add SHA-256 computation
   - Skip unchanged files

4. **Session Persistence** (0.5 day)
   - Actually insert session_events
   - Enable temporal queries

### Phase 2 (Week 4-5) - Neural Intelligence

**Priority: MEDIUM** (Core functionality)

5. **ONNX Integration** (2 days)
   - Add onnxruntime dependency
   - Load GLiNER model
   - Implement entity extraction

6. **Polymorphic Schema** (1 day)
   - Split nodes into resources + entities
   - Add source_type/target_type to edges
   - Update all queries

7. **sqlite-vec Integration** (1 day)
   - Load extension
   - Create virtual table
   - Implement vector search

### Phase 3 (Week 6-8) - Visualization

**Priority: LOW** (User experience)

8. **Graph Renderer** (3 days)
   - Choose framework (Tauri vs PyQt6)
   - Embed web view
   - Integrate Sigma.js

9. **Bridge Layer** (2 days)
   - Implement data serialization
   - Connect SQL ↔ JavaScript
   - Handle user interactions

### Phase 4 (Month 3+) - Production Hardening

10. **Multi-Process Architecture** (2 days)
    - Separate indexer process
    - Event queue with batching
    - Error recovery

---

## Immediate Action Items

**Before continuing with new features:**

1. ✅ Run current tests to verify baseline
2. ⚠️ Fix relative path storage
3. ⚠️ Add debounced observer
4. ⚠️ Implement content hashing
5. ⚠️ Persist session events

**These 4 fixes are CRITICAL for spec compliance and prevent data loss.**

---

## Testing Strategy

### Current Tests (Passing)
- ✅ populate_graph: Creates nodes/edges
- ✅ watch_files: Observer creates edges

### New Tests Required
1. **test_relative_paths**: Move database, verify paths still work
2. **test_atomic_write**: Save file in VSCode, verify single event
3. **test_content_hash**: Edit file, verify re-index only if changed
4. **test_entity_extraction**: Extract "Alice" from text → PERSON
5. **test_vector_search**: Find similar documents via embeddings
6. **test_cycle_prevention**: Traverse graph, no infinite loops

---

## Dependency Additions

**For ONNX:**
```toml
onnxruntime = "1.16"
tokenizers = "0.15"
```

**For sqlite-vec:**
```toml
# Requires building from source or system install
# https://github.com/asg017/sqlite-vec
```

**For UI (if Rust):**
```toml
tauri = "2.0"
serde_json = "1.0"
```

---

## Risk Assessment

**High Risk:**
- ONNX integration may require Python (no mature Rust bindings)
- sqlite-vec requires C compilation (build complexity)
- Multi-process in Rust harder than Python (no multiprocessing module)

**Mitigation:**
- Create Python subprocess for ONNX inference
- Bundle pre-compiled sqlite-vec
- Use tokio channels instead of processes

---

## Spec Compliance Score

**Current:** 40% (4/10 major features)

**After Phase 1.5:** 70% (7/10)

**After Phase 2:** 90% (9/10)

**After Phase 3:** 100% (Full compliance)

---

## Next Steps

1. Read IMPLEMENTATION_GAPS.md (this file)
2. Implement Phase 1.5 fixes
3. Run expanded test suite
4. Benchmark performance
5. Move to Phase 2

**Do NOT add new features until Phase 1.5 complete.**
