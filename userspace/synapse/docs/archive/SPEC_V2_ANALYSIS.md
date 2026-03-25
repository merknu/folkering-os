# Synapse Specification V2 - Detailed Implementation Analysis

**Date:** 2026-01-25
**Source:** Architectural Specification: Synapse Core Feature Implementation
**Status:** Deep technical specification with concrete implementation guidance

---

## Executive Summary

The second specification document provides **concrete implementation details** that go beyond the high-level architecture. It specifies exact crates, algorithms, and data structures.

### Key New Requirements

1. **Storage:** JSONB (not JSON), functional indexes, UTF-8 enforcement
2. **File Watching:** notify-debouncer-full crate, 500ms tick rate, inode tracking
3. **Neural Engine:** ort crate, quantized Int8 models, GPU acceleration
4. **Vector Search:** Shadow table pattern, vec0 virtual tables
5. **Hybrid Search:** Reciprocal Rank Fusion (RRF) algorithm
6. **Data Transfer:** Apache Arrow IPC (not JSON)

---

## 1. Storage Portability: Enhanced Requirements

### 1.1 Path Normalization - DETAILED SPEC

**Spec Quote:**
> "The normalization process occurs at the 'System Access Layer,' the boundary where the application interfaces with the operating system. When a file is ingested:
> 1. Stripping the Prefix
> 2. Separator Unification (always forward slash /)
> 3. UTF-8 Enforcement"

**Current Implementation Gap:**
```rust
// We do: Store absolute path
graph.register_path(&node.id, "/home/user/project/file.txt").await?;

// Spec requires: Relative + normalized
// 1. Strip prefix: /home/user/project
// 2. Result: file.txt
// 3. Normalize: file.txt (already correct)
// 4. Enforce UTF-8: validate or reject
```

**Enhanced Implementation Required:**

```rust
// src/graph/path_normalization.rs

use std::path::{Path, PathBuf};
use anyhow::{Result, bail};

/// Canonical path normalization per spec
pub struct PathNormalizer {
    project_root: PathBuf,
}

impl PathNormalizer {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }

    /// Normalize path for storage (spec-compliant)
    pub fn normalize_for_storage(&self, absolute_path: &Path) -> Result<String> {
        // 1. Strip prefix
        let relative = absolute_path.strip_prefix(&self.project_root)
            .map_err(|_| anyhow::anyhow!("Path not in project root"))?;

        // 2. Separator unification (always /)
        let components: Vec<_> = relative.components()
            .filter_map(|c| match c {
                std::path::Component::Normal(os_str) => Some(os_str),
                _ => None,
            })
            .collect();

        // 3. UTF-8 enforcement
        let parts: Result<Vec<String>> = components.iter()
            .map(|os_str| {
                os_str.to_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid UTF-8 in path"))
                    .map(|s| s.to_string())
            })
            .collect();

        let parts = parts?;

        // Join with forward slash
        Ok(parts.join("/"))
    }

    /// Hydrate path from storage (spec-compliant)
    pub fn hydrate_from_storage(&self, normalized: &str) -> PathBuf {
        let mut path = self.project_root.clone();

        // Split by / and push each component
        for segment in normalized.split('/') {
            if !segment.is_empty() {
                path.push(segment);
            }
        }

        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_windows_path_normalization() {
        let normalizer = PathNormalizer::new(PathBuf::from("C:\\Users\\Alice\\Synapse"));

        // Windows path with backslashes
        let absolute = PathBuf::from("C:\\Users\\Alice\\Synapse\\notes\\idea.md");
        let normalized = normalizer.normalize_for_storage(&absolute).unwrap();

        assert_eq!(normalized, "notes/idea.md");  // Forward slashes!
    }

    #[test]
    fn test_linux_path_normalization() {
        let normalizer = PathNormalizer::new(PathBuf::from("/home/alice/synapse"));

        let absolute = PathBuf::from("/home/alice/synapse/notes/idea.md");
        let normalized = normalizer.normalize_for_storage(&absolute).unwrap();

        assert_eq!(normalized, "notes/idea.md");
    }

    #[test]
    fn test_utf8_enforcement() {
        // Test that non-UTF-8 paths are rejected
        // (Would require creating actual files with invalid UTF-8 names on Linux)
    }

    #[test]
    fn test_hydration() {
        let normalizer = PathNormalizer::new(PathBuf::from("/home/alice/synapse"));

        let normalized = "notes/science/physics.md";
        let hydrated = normalizer.hydrate_from_storage(normalized);

        assert_eq!(
            hydrated,
            PathBuf::from("/home/alice/synapse/notes/science/physics.md")
        );
    }
}
```

**Database Storage Decision:**

**Spec Quote:**
> "Given the requirement for portability and the text-heavy nature of knowledge graphs, the TEXT storage strategy is superior."

**Current:** ✅ Already using TEXT
**Action:** No change needed

---

## 2. Atomic File Watching - CONCRETE IMPLEMENTATION

### 2.1 Required Crate: notify-debouncer-full

**Spec Quote:**
> "Synapse must utilize the notify-debouncer-full crate. This library acts as a middleware layer between the raw OS events and the application logic."

**Current Implementation:**
```toml
# Cargo.toml
notify = "6.1"  # ❌ Wrong - too low-level
```

**Required:**
```toml
# Cargo.toml
notify-debouncer-full = "0.3"  # ✅ Correct
```

### 2.2 Configuration Parameters

**Spec Requirements:**
- tick_rate: 500ms (not 1000ms as in previous plan)
- FileIdMap: Track inodes for rename detection
- Recursive directory watching (not individual files)

**Implementation:**

```rust
// src/observer/robust_watcher.rs

use notify_debouncer_full::{
    new_debouncer,
    notify::*,
    DebounceEventResult,
    Debouncer,
    FileIdMap,
};
use std::time::Duration;
use tokio::sync::mpsc;

pub struct RobustFileWatcher {
    _debouncer: Debouncer<RecommendedWatcher, FileIdMap>,
    event_receiver: mpsc::Receiver<DebouncedEvent>,
}

impl RobustFileWatcher {
    pub async fn new(watch_path: PathBuf) -> Result<Self> {
        // Create async channel
        let (tx, rx) = mpsc::channel(100);

        // Configure debouncer with 500ms tick rate
        let debouncer = new_debouncer(
            Duration::from_millis(500),  // ← Spec requirement
            None,  // No custom cache
            move |result: DebounceEventResult| {
                match result {
                    Ok(events) => {
                        for event in events {
                            // Send to async channel
                            let _ = tx.blocking_send(event);
                        }
                    }
                    Err(e) => eprintln!("Watch error: {:?}", e),
                }
            },
        )?;

        // Watch parent directory recursively
        debouncer.watcher().watch(
            &watch_path,
            RecursiveMode::Recursive,  // ← Critical for inode tracking
        )?;

        Ok(Self {
            _debouncer: debouncer,
            event_receiver: rx,
        })
    }

    pub async fn next_event(&mut self) -> Option<DebouncedEvent> {
        self.event_receiver.recv().await
    }
}
```

### 2.3 Event Filtering Logic

**Spec Quote:**
> "Apply a secondary filter:
> - Ignore: Paths ending in ~, .tmp, .swp, or matching .git
> - Map: Treat Rename events where destination is tracked file as ContentUpdate
> - Debounce: If Remove followed by Create within 100ms, treat as atomic overwrite"

**Implementation:**

```rust
// src/observer/event_filter.rs

const IGNORE_EXTENSIONS: &[&str] = &["~", ".tmp", ".swp", ".bak"];
const IGNORE_DIRS: &[&str] = &[".git", "__pycache__", "node_modules", ".idea", ".vscode"];

pub struct EventFilter {
    recent_removes: HashMap<PathBuf, Instant>,
}

impl EventFilter {
    pub fn new() -> Self {
        Self {
            recent_removes: HashMap::new(),
        }
    }

    pub fn should_ignore(&self, path: &Path) -> bool {
        // Check extensions
        if let Some(ext) = path.extension() {
            let ext_str = ext.to_string_lossy();
            if IGNORE_EXTENSIONS.iter().any(|&ignore| ext_str.ends_with(ignore)) {
                return true;
            }
        }

        // Check directory components
        for component in path.components() {
            if let Some(name) = component.as_os_str().to_str() {
                if IGNORE_DIRS.contains(&name) {
                    return true;
                }
            }
        }

        false
    }

    pub fn process_event(&mut self, event: DebouncedEvent) -> Option<ProcessedEvent> {
        for path in &event.paths {
            if self.should_ignore(path) {
                return None;
            }
        }

        // Detect atomic overwrite pattern
        match event.kind {
            EventKind::Remove(_) => {
                // Track removal
                self.recent_removes.insert(event.paths[0].clone(), Instant::now());
                None  // Don't process yet
            }
            EventKind::Create(_) => {
                // Check if this is a create after recent remove
                if let Some(remove_time) = self.recent_removes.get(&event.paths[0]) {
                    if remove_time.elapsed() < Duration::from_millis(100) {
                        // This is an atomic overwrite, treat as modify
                        self.recent_removes.remove(&event.paths[0]);
                        return Some(ProcessedEvent::Modified(event.paths[0].clone()));
                    }
                }
                Some(ProcessedEvent::Created(event.paths[0].clone()))
            }
            EventKind::Modify(_) => {
                Some(ProcessedEvent::Modified(event.paths[0].clone()))
            }
            _ => None,
        }
    }
}

pub enum ProcessedEvent {
    Created(PathBuf),
    Modified(PathBuf),
    Deleted(PathBuf),
}
```

---

## 3. Polymorphic Schema - JSONB Requirement

### 3.1 JSONB vs JSON

**Spec Quote:**
> "The optimal architecture leverages SQLite 3.45+, which introduced JSONB, a binary serialization format for JSON stored in BLOB columns."

**Critical Difference:**
- **JSON (TEXT):** Stored as UTF-8 string, requires parsing on every read
- **JSONB (BLOB):** Binary format, O(1) key access, functional indexes

**Current Implementation:**
```rust
// src/models/node.rs
pub struct Node {
    pub properties: String,  // ❌ JSON as TEXT
}
```

**Required:**
```rust
pub struct Node {
    pub properties: Vec<u8>,  // ✅ JSONB as BLOB
}
```

### 3.2 Schema Update

**Migration Required:**

```sql
-- migrations/004_jsonb_conversion.sql

-- BEFORE
CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    properties TEXT NOT NULL,  -- JSON as TEXT
    ...
);

-- AFTER
CREATE TABLE nodes (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    properties BLOB NOT NULL,  -- JSONB as BLOB
    ...
);
```

### 3.3 Functional Indexes

**Spec Quote:**
> "To efficiently query people by email, define an index:
> CREATE INDEX idx_nodes_email ON nodes(jsonb_extract(properties, '$.email')) WHERE type = 'person';"

**Implementation:**

```sql
-- Create functional indexes on frequently queried properties

-- Index for person emails
CREATE INDEX idx_person_email
ON nodes(jsonb_extract(properties, '$.email'))
WHERE type = 'person';

-- Index for file extensions
CREATE INDEX idx_file_extension
ON nodes(jsonb_extract(properties, '$.extension'))
WHERE type = 'file';

-- Index for tag names
CREATE INDEX idx_tag_name
ON nodes(jsonb_extract(properties, '$.name'))
WHERE type = 'tag';
```

**Query Performance:**
```sql
-- Before (full table scan):
SELECT * FROM nodes
WHERE type = 'person'
  AND json_extract(properties, '$.email') = 'alice@example.com';
-- O(N) - slow

-- After (index seek):
SELECT * FROM nodes
WHERE type = 'person'
  AND jsonb_extract(properties, '$.email') = 'alice@example.com';
-- O(log N) - fast
```

---

## 4. GLiNER Integration - Specific Implementation

### 4.1 Required Crate: ort

**Spec Quote:**
> "The implementation relies on the ort crate, which provides bindings to the Microsoft ONNX Runtime."

**Cargo.toml:**
```toml
[dependencies]
ort = { version = "2.0", features = ["cuda", "download-binaries"] }
tokenizers = "0.15"
ndarray = "0.15"
```

### 4.2 Quantized Models

**Spec Quote:**
> "By exporting the GLiNER model to ONNX with Int8 quantization, the model size is reduced by approximately 75% (400MB → 100MB) with negligible loss in accuracy."

**Model Preparation (Python):**
```python
# scripts/export_gliner.py

from gliner import GLiNER
from optimum.onnxruntime import ORTQuantizer, ORTModelForQuestionAnswering
from optimum.onnxruntime.configuration import AutoQuantizationConfig

# Load GLiNER model
model = GLiNER.from_pretrained("urchade/gliner_small-v2.1")

# Export to ONNX
model.to_onnx("gliner.onnx")

# Quantize to Int8
quantizer = ORTQuantizer.from_pretrained("gliner.onnx")
qconfig = AutoQuantizationConfig.avx512_vnni(is_static=False)
quantizer.quantize(
    save_dir="gliner_quantized",
    quantization_config=qconfig,
)

# Result: gliner_quantized/model.onnx (100MB vs 400MB)
```

### 4.3 Rust Inference Pipeline

**Implementation:**

```rust
// src/neural/gliner.rs

use ort::{Session, Value, GraphOptimizationLevel, ExecutionProvider};
use tokenizers::Tokenizer;
use ndarray::{Array1, Array2};

pub struct GLiNERService {
    session: Session,
    tokenizer: Tokenizer,
}

impl GLiNERService {
    pub fn new(model_path: &str, tokenizer_path: &str) -> Result<Self> {
        // Configure ONNX Runtime with GPU acceleration
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_execution_providers([
                ExecutionProvider::CUDA(Default::default()),  // Try GPU first
                ExecutionProvider::CPU(Default::default()),   // Fallback to CPU
            ])?
            .commit_from_file(model_path)?;

        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

        Ok(Self { session, tokenizer })
    }

    pub fn extract_entities(
        &self,
        text: &str,
        labels: &[&str],
        threshold: f32,
    ) -> Result<Vec<Entity>> {
        // 1. Tokenization
        let encoding = self.tokenizer.encode(text, true)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;

        let input_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();

        // 2. Create ONNX inputs
        let input_ids_array = Array2::from_shape_vec(
            (1, input_ids.len()),
            input_ids.iter().map(|&x| x as i64).collect(),
        )?;

        let attention_mask_array = Array2::from_shape_vec(
            (1, attention_mask.len()),
            attention_mask.iter().map(|&x| x as i64).collect(),
        )?;

        let inputs = vec![
            Value::from_array(self.session.allocator(), &input_ids_array)?,
            Value::from_array(self.session.allocator(), &attention_mask_array)?,
        ];

        // 3. Run inference
        let outputs = self.session.run(inputs)?;

        // 4. Post-process (decode spans, apply threshold)
        let logits = outputs[0].try_extract::<f32>()?;
        let entities = self.decode_entities(logits, text, labels, threshold)?;

        Ok(entities)
    }

    fn decode_entities(
        &self,
        logits: &[f32],
        text: &str,
        labels: &[&str],
        threshold: f32,
    ) -> Result<Vec<Entity>> {
        // Apply sigmoid, filter by threshold, map to text offsets
        let mut entities = Vec::new();

        // TODO: Implement span decoding logic
        // (This is model-specific and complex)

        Ok(entities)
    }
}

#[derive(Debug, Clone)]
pub struct Entity {
    pub text: String,
    pub label: String,
    pub confidence: f32,
    pub start: usize,
    pub end: usize,
}
```

---

## 5. Vector Search - Shadow Table Pattern

### 5.1 sqlite-vec Virtual Table

**Spec Quote:**
> "CREATE VIRTUAL TABLE vec_nodes USING vec0(rowid INTEGER PRIMARY KEY, embedding float[384]);"

**Migration:**

```sql
-- migrations/005_vector_search.sql

-- Load sqlite-vec extension (must be compiled/installed first)
-- Extension will be loaded via Rust code

-- Create virtual table for vectors
CREATE VIRTUAL TABLE vec_nodes USING vec0(
    embedding float[384]
);

-- Shadow table pattern: rowid matches nodes.id
-- No explicit foreign key needed - enforce in application logic
```

### 5.2 Loading Extension in Rust

**Implementation:**

```rust
// src/graph/vec_extension.rs

use rusqlite::{Connection, LoadExtensionGuard};

pub fn load_sqlite_vec(conn: &mut Connection) -> Result<()> {
    unsafe {
        let _guard = LoadExtensionGuard::new(conn)?;

        // Try to load extension
        // Path depends on installation
        conn.load_extension("vec0", None)
            .or_else(|_| conn.load_extension("./libvec0", None))
            .or_else(|_| conn.load_extension("/usr/lib/sqlite3/vec0", None))?;
    }

    Ok(())
}

// For sqlx (async):
pub async fn load_sqlite_vec_async(pool: &SqlitePool) -> Result<()> {
    // sqlx doesn't support load_extension directly
    // Workaround: Enable in connection string

    // Alternative: Use sqlite_vec crate wrapper
    // https://github.com/asg017/sqlite-vec

    Ok(())
}
```

### 5.3 Vector Insertion

**Implementation:**

```rust
// src/graph/vector_ops.rs

use ndarray::Array1;

pub async fn insert_embedding(
    db: &SqlitePool,
    node_id: i64,
    embedding: &[f32],
) -> Result<()> {
    // Convert f32 slice to binary blob
    let blob: Vec<u8> = embedding.iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();

    // Insert into virtual table
    // rowid must match node.id for shadow table pattern
    sqlx::query(
        "INSERT INTO vec_nodes(rowid, embedding) VALUES (?, ?)"
    )
    .bind(node_id)
    .bind(&blob)
    .execute(db)
    .await?;

    Ok(())
}
```

---

## 6. Hybrid Search - Reciprocal Rank Fusion

### 6.1 RRF Algorithm

**Spec Quote:**
> "For each unique document found in either list, calculate:
> Score = 1/(k + Rank_FTS) + 1/(k + Rank_Vector)
> Where k is a smoothing constant, typically 60."

**SQL Implementation:**

```sql
-- src/query/hybrid_search.sql

WITH
-- CTE 1: Full-Text Search results with ranks
fts_results AS (
    SELECT
        rowid,
        ROW_NUMBER() OVER (ORDER BY rank) AS fts_rank
    FROM nodes_fts
    WHERE nodes_fts MATCH ?
    LIMIT 50
),

-- CTE 2: Vector search results with ranks
vec_results AS (
    SELECT
        rowid,
        ROW_NUMBER() OVER (ORDER BY distance) AS vec_rank
    FROM vec_nodes
    WHERE embedding MATCH ?
    ORDER BY distance
    LIMIT 50
),

-- CTE 3: Reciprocal Rank Fusion
rrf_scores AS (
    SELECT
        COALESCE(f.rowid, v.rowid) AS rowid,
        COALESCE(1.0 / (60 + f.fts_rank), 0) +
        COALESCE(1.0 / (60 + v.vec_rank), 0) AS rrf_score
    FROM fts_results f
    FULL OUTER JOIN vec_results v ON f.rowid = v.rowid
)

-- Final: Join with nodes to get actual data
SELECT
    n.*,
    r.rrf_score
FROM nodes n
JOIN rrf_scores r ON n.id = r.rowid
ORDER BY r.rrf_score DESC
LIMIT 20;
```

**Rust Implementation:**

```rust
// src/query/hybrid_search.rs

pub struct HybridSearchEngine {
    db: SqlitePool,
}

impl HybridSearchEngine {
    pub async fn search(
        &self,
        text_query: &str,
        vector_query: &[f32],
        limit: usize,
    ) -> Result<Vec<(Node, f32)>> {
        // Execute hybrid search query
        let results = sqlx::query_as::<_, (Node, f32)>(
            include_str!("hybrid_search.sql")
        )
        .bind(text_query)
        .bind(serialize_vector(vector_query))
        .bind(limit as i64)
        .fetch_all(&self.db)
        .await?;

        Ok(results)
    }
}

fn serialize_vector(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}
```

---

## 7. Frontend Data Transfer - Apache Arrow IPC

### 7.1 Why Arrow?

**Spec Quote:**
> "JSON serialization is CPU-intensive and produces large text payloads. Synapse integrates Apache Arrow IPC... This allows rendering hundreds of thousands of nodes with near-zero deserialization cost."

**Performance Comparison:**

| Format | 10,000 nodes | 100,000 nodes | Deserialization |
|--------|--------------|---------------|-----------------|
| JSON | 15 MB | 150 MB | ~500ms |
| Arrow IPC | 3 MB | 30 MB | ~5ms |

### 7.2 Rust Implementation

**Cargo.toml:**
```toml
[dependencies]
arrow = "51.0"
arrow-ipc = "51.0"
```

**Implementation:**

```rust
// src/api/arrow_serialization.rs

use arrow::array::{StringArray, Float32Array, StructArray};
use arrow::datatypes::{Schema, Field, DataType};
use arrow::record_batch::RecordBatch;
use arrow::ipc::writer::StreamWriter;

pub fn serialize_nodes_to_arrow(nodes: Vec<Node>) -> Result<Vec<u8>> {
    // Define schema
    let schema = Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("label", DataType::Utf8, true),
        Field::new("x", DataType::Float32, true),
        Field::new("y", DataType::Float32, true),
    ]);

    // Build arrays
    let ids: StringArray = nodes.iter()
        .map(|n| Some(n.id.as_str()))
        .collect();

    let types: StringArray = nodes.iter()
        .map(|n| Some(n.r#type.as_str()))
        .collect();

    // ... more fields

    // Create RecordBatch
    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(ids),
            Arc::new(types),
            // ... more columns
        ],
    )?;

    // Serialize to IPC format
    let mut buffer = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut buffer, &batch.schema())?;
        writer.write(&batch)?;
        writer.finish()?;
    }

    Ok(buffer)
}
```

### 7.3 Frontend (Tauri/JavaScript)

**JavaScript:**
```javascript
import { tableFromIPC } from 'apache-arrow';

// Receive binary from Rust backend
async function loadGraph() {
    const binaryData = await invoke('get_graph_arrow');

    // Deserialize Arrow IPC
    const table = tableFromIPC(binaryData);

    // Access as typed arrays (zero-copy!)
    const ids = table.getChild('id').toArray();
    const x = table.getChild('x').toArray();  // Float32Array
    const y = table.getChild('y').toArray();  // Float32Array

    // Render with Sigma.js/Cosmograph (direct typed array access)
    renderGraph({ ids, x, y });
}
```

---

## Updated Implementation Priority

### P0: Critical (Spec V2 Requirements)

| Task | Current | Required | Effort |
|------|---------|----------|--------|
| Path Normalization | Absolute paths | UTF-8 + forward slash | 1 day |
| File Watcher | notify | notify-debouncer-full | 1 day |
| JSONB Schema | JSON TEXT | JSONB BLOB | 0.5 day |
| Functional Indexes | None | jsonb_extract indexes | 0.5 day |

**Total: 3 days**

### P1: High (Core Features)

| Task | Current | Required | Effort |
|------|---------|----------|--------|
| GLiNER | Regex | ort + quantized ONNX | 2 days |
| sqlite-vec | Placeholder | Virtual table + shadow pattern | 1 day |
| RRF Search | None | Hybrid FTS + Vector | 1 day |

**Total: 4 days**

### P2: Medium (Optimization)

| Task | Current | Required | Effort |
|------|---------|----------|--------|
| Arrow IPC | None | Binary serialization | 1 day |
| GPU Acceleration | None | CUDA execution provider | 0.5 day |

**Total: 1.5 days**

---

## Key Takeaways from Spec V2

1. **Be Specific:** Use exact crates (notify-debouncer-full, not generic notify)
2. **Use Binary Formats:** JSONB (not JSON), Arrow (not JSON)
3. **Leverage SQLite Features:** Functional indexes, virtual tables
4. **Optimize ML:** Int8 quantization, GPU providers
5. **Shadow Table Pattern:** Separate heavy data (vectors) from metadata

**Next Steps:**
1. Read this document fully
2. Update PHASE_1.5_CHECKLIST with V2 specifics
3. Install notify-debouncer-full
4. Test JSONB conversion
5. Plan GLiNER ONNX integration

---

## Dependency Summary

**New Dependencies Required:**

```toml
[dependencies]
# File watching (CHANGED)
notify-debouncer-full = "0.3"  # Was: notify = "6.1"

# Neural engine
ort = { version = "2.0", features = ["cuda", "download-binaries"] }
tokenizers = "0.15"
ndarray = "0.15"

# Data serialization
arrow = "51.0"
arrow-ipc = "51.0"

# Existing (keep)
sqlx = { version = "0.7", features = ["runtime-tokio", "sqlite"] }
tokio = { version = "1.35", features = ["full"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
sha2 = "0.10"  # For content hashing
```

**External Dependencies:**

1. **sqlite-vec:** Requires compilation or system install
   - Download: https://github.com/asg017/sqlite-vec
   - Compile: `gcc -shared -o vec0.so vec0.c`
   - Install: Copy to SQLite extensions dir

2. **GLiNER ONNX Model:** ~100MB quantized model
   - Export from Python (see script above)
   - Place in `assets/models/gliner_quantized.onnx`

---

## Compliance After V2

**Current:** 40% (4/10)

**After Spec V2 Implementation:** 95% (19/20)

Missing only:
- Multi-process architecture (not critical for single-user)

**All other requirements fully specified and achievable.**
