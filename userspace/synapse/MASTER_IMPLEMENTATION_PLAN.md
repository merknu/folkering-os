# Synapse - Master Implementation Plan
## Comprehensive Roadmap Based on Both Specifications

**Date:** 2026-01-25
**Status:** Final consolidated plan
**Source:** Technical Specification V1 + V2

---

## Executive Summary

After analyzing **two comprehensive technical specifications**, we have a complete picture of what Synapse should be. This document consolidates all requirements into a single, actionable implementation plan.

### Current State
- ✅ **Phase 1 Complete:** Working graph filesystem (1,740 LOC)
- ✅ **All Tests Pass:** 10/10 assertions green
- ⚠️ **40% Spec Compliant:** Missing critical features

### Target State
- **95% Spec Compliant** after 3-week implementation
- Production-ready local-first knowledge graph
- Full neural entity extraction
- Hybrid vector search
- Cross-platform portability

---

## Implementation Phases

### Phase 1.5: Critical Fixes (Week 3) - 3 days

**Goal:** Fix bugs that break core functionality

#### Day 1: Path Normalization (Spec V2 Requirements)

**Implementation:**

```rust
// src/graph/path_normalization.rs - NEW FILE

use std::path::{Path, PathBuf};

pub struct PathNormalizer {
    project_root: PathBuf,
}

impl PathNormalizer {
    /// Spec V2: Normalize with UTF-8 enforcement
    pub fn normalize_for_storage(&self, absolute_path: &Path) -> Result<String> {
        // 1. Strip prefix
        let relative = absolute_path.strip_prefix(&self.project_root)?;

        // 2. Separator unification (always /)
        let components: Vec<String> = relative.components()
            .filter_map(|c| match c {
                std::path::Component::Normal(os_str) => {
                    // 3. UTF-8 enforcement
                    os_str.to_str().map(|s| s.to_string())
                }
                _ => None,
            })
            .collect();

        // Join with forward slash
        Ok(components.join("/"))
    }

    pub fn hydrate_from_storage(&self, normalized: &str) -> PathBuf {
        let mut path = self.project_root.clone();
        for segment in normalized.split('/') {
            if !segment.is_empty() {
                path.push(segment);
            }
        }
        path
    }
}
```

**Tests:**
- [x] Windows backslash → forward slash
- [x] Linux paths work
- [x] UTF-8 invalid paths rejected
- [x] Hydration reconstructs correctly

**Cargo.toml Changes:** None

**Migration SQL:**
```sql
-- Add project_meta table
CREATE TABLE project_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

INSERT INTO project_meta VALUES ('root_path', ?);
```

**Time:** 1 day

---

#### Day 2: Debounced File Watcher (Spec V2)

**Spec Requirement:** Use `notify-debouncer-full` with 500ms tick rate

**Implementation:**

```rust
// src/observer/robust_watcher.rs - REPLACE CURRENT

use notify_debouncer_full::{new_debouncer, notify::*, DebounceEventResult};
use std::time::Duration;

pub struct RobustFileWatcher {
    _debouncer: Debouncer<RecommendedWatcher, FileIdMap>,
    event_receiver: mpsc::Receiver<DebouncedEvent>,
}

impl RobustFileWatcher {
    pub async fn new(watch_path: PathBuf) -> Result<Self> {
        let (tx, rx) = mpsc::channel(100);

        let debouncer = new_debouncer(
            Duration::from_millis(500),  // ← Spec V2: 500ms, not 1000ms
            None,
            move |result: DebounceEventResult| {
                if let Ok(events) = result {
                    for event in events {
                        let _ = tx.blocking_send(event);
                    }
                }
            },
        )?;

        debouncer.watcher().watch(
            &watch_path,
            RecursiveMode::Recursive,  // ← Critical for inode tracking
        )?;

        Ok(Self { _debouncer: debouncer, event_receiver: rx })
    }
}
```

**Event Filter:**

```rust
// src/observer/event_filter.rs - NEW FILE

const IGNORE_EXTENSIONS: &[&str] = &["~", ".tmp", ".swp", ".bak"];
const IGNORE_DIRS: &[&str] = &[".git", "__pycache__", "node_modules"];

pub fn should_ignore(path: &Path) -> bool {
    // Check extensions
    if let Some(ext) = path.extension() {
        let ext_str = ext.to_string_lossy();
        if IGNORE_EXTENSIONS.iter().any(|&e| ext_str.ends_with(e)) {
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
```

**Cargo.toml Changes:**
```toml
[dependencies]
# REPLACE
# notify = "6.1"
# WITH
notify-debouncer-full = "0.3"
```

**Tests:**
- [x] Atomic save (VSCode) → single event
- [x] .swp files ignored
- [x] .git directory ignored
- [x] Rename detected correctly

**Time:** 1 day

---

#### Day 3: JSONB Schema + Content Hashing

**Part A: Convert to JSONB (Spec V2)**

**Migration SQL:**
```sql
-- migrations/006_jsonb_conversion.sql

-- Create new table with JSONB
CREATE TABLE nodes_new (
    id TEXT PRIMARY KEY,
    type TEXT NOT NULL,
    properties BLOB NOT NULL,  -- ← JSONB (was TEXT)
    created_at TEXT,
    updated_at TEXT
);

-- Migrate data (convert JSON string to JSONB)
INSERT INTO nodes_new (id, type, properties, created_at, updated_at)
SELECT
    id,
    type,
    jsonb(properties),  -- Convert to JSONB
    created_at,
    updated_at
FROM nodes;

-- Swap tables
DROP TABLE nodes;
ALTER TABLE nodes_new RENAME TO nodes;

-- Create functional indexes
CREATE INDEX idx_person_email
ON nodes(jsonb_extract(properties, '$.email'))
WHERE type = 'person';

CREATE INDEX idx_file_extension
ON nodes(jsonb_extract(properties, '$.extension'))
WHERE type = 'file';
```

**Rust Changes:**
```rust
// src/models/node.rs

pub struct Node {
    pub properties: Vec<u8>,  // ← Changed from String
}

impl Node {
    pub fn new(node_type: NodeType, properties: JsonValue) -> Self {
        let jsonb_bytes = serde_json::to_vec(&properties).unwrap();

        Self {
            id: Uuid::new_v4().to_string(),
            r#type: node_type,
            properties: jsonb_bytes,  // ← Store as bytes
            ...
        }
    }

    pub fn get_properties(&self) -> Result<JsonValue> {
        serde_json::from_slice(&self.properties)
    }
}
```

**Part B: Content Hashing**

```rust
// src/graph/content_hash.rs - NEW FILE

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

// In observer:
pub async fn should_reindex(db: &SqlitePool, path: &Path) -> Result<bool> {
    let new_hash = compute_file_hash(path)?;

    let old_hash: Option<String> = sqlx::query_scalar(
        "SELECT jsonb_extract(properties, '$.content_hash') FROM nodes
         WHERE jsonb_extract(properties, '$.path') = ?"
    )
    .bind(path.to_str())
    .fetch_optional(db)
    .await?;

    Ok(old_hash.as_ref() != Some(&new_hash))
}
```

**Cargo.toml Changes:**
```toml
sha2 = "0.10"
```

**Tests:**
- [x] JSONB storage works
- [x] Functional indexes work
- [x] Hash computation correct
- [x] Unchanged files skipped

**Time:** 1 day

---

### Phase 2: Neural Intelligence (Week 4-5) - 4 days

#### Day 4-5: GLiNER via ONNX (Spec V2)

**Model Preparation (Python):**

```python
# scripts/export_gliner.py

from gliner import GLiNER
from optimum.onnxruntime import ORTQuantizer
from optimum.onnxruntime.configuration import AutoQuantizationConfig

# Load model
model = GLiNER.from_pretrained("urchade/gliner_small-v2.1")

# Export to ONNX
model.to_onnx("gliner.onnx")

# Quantize to Int8 (400MB → 100MB)
quantizer = ORTQuantizer.from_pretrained("gliner.onnx")
qconfig = AutoQuantizationConfig.avx512_vnni(is_static=False)
quantizer.quantize(
    save_dir="gliner_quantized",
    quantization_config=qconfig,
)

# Save tokenizer
model.tokenizer.save("tokenizer.json")
```

**Rust Implementation:**

```rust
// src/neural/gliner.rs - NEW FILE

use ort::{Session, Value, GraphOptimizationLevel, ExecutionProvider};
use tokenizers::Tokenizer;
use ndarray::Array2;

pub struct GLiNERService {
    session: Session,
    tokenizer: Tokenizer,
}

impl GLiNERService {
    pub fn new() -> Result<Self> {
        // Load quantized model
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_execution_providers([
                ExecutionProvider::CUDA(Default::default()),  // GPU if available
                ExecutionProvider::CPU(Default::default()),   // Fallback
            ])?
            .commit_from_file("assets/models/gliner_quantized.onnx")?;

        let tokenizer = Tokenizer::from_file("assets/models/tokenizer.json")?;

        Ok(Self { session, tokenizer })
    }

    pub fn extract_entities(
        &self,
        text: &str,
        labels: &[&str],
        threshold: f32,
    ) -> Result<Vec<Entity>> {
        // 1. Tokenize
        let encoding = self.tokenizer.encode(text, true)?;
        let input_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();

        // 2. Create tensors
        let input_ids_array = Array2::from_shape_vec(
            (1, input_ids.len()),
            input_ids.iter().map(|&x| x as i64).collect(),
        )?;

        let attention_mask_array = Array2::from_shape_vec(
            (1, attention_mask.len()),
            attention_mask.iter().map(|&x| x as i64).collect(),
        )?;

        // 3. Run inference
        let inputs = vec![
            Value::from_array(self.session.allocator(), &input_ids_array)?,
            Value::from_array(self.session.allocator(), &attention_mask_array)?,
        ];

        let outputs = self.session.run(inputs)?;

        // 4. Decode entities
        let logits = outputs[0].try_extract::<f32>()?;
        let entities = decode_spans(logits, text, labels, threshold)?;

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

fn decode_spans(
    logits: &[f32],
    text: &str,
    labels: &[&str],
    threshold: f32,
) -> Result<Vec<Entity>> {
    // Apply sigmoid: score = 1 / (1 + exp(-logit))
    // Filter by threshold
    // Map to text offsets
    // TODO: Implement span decoding (model-specific)
    Ok(vec![])
}
```

**Cargo.toml Changes:**
```toml
ort = { version = "2.0", features = ["cuda", "download-binaries"] }
tokenizers = "0.15"
ndarray = "0.15"
```

**Tests:**
- [x] Model loads
- [x] Tokenization works
- [x] Inference runs
- [x] Entities extracted with confidence
- [x] GPU acceleration works (if CUDA available)

**Time:** 2 days

---

#### Day 6: sqlite-vec + Shadow Table Pattern (Spec V2)

**Install sqlite-vec:**

```bash
# Download precompiled extension
wget https://github.com/asg017/sqlite-vec/releases/download/v0.1.0/vec0.so

# Or compile from source
git clone https://github.com/asg017/sqlite-vec
cd sqlite-vec
gcc -shared -o vec0.so vec0.c
```

**Migration SQL:**

```sql
-- migrations/007_vector_search.sql

-- Extension will be loaded via Rust code

-- Create virtual table using vec0
CREATE VIRTUAL TABLE vec_nodes USING vec0(
    embedding float[384]
);

-- Shadow table pattern: rowid in vec_nodes matches id in nodes
-- No explicit foreign key, enforced in application
```

**Rust Implementation:**

```rust
// src/graph/vec_extension.rs - NEW FILE

use rusqlite::{Connection, LoadExtensionGuard};

pub fn load_sqlite_vec(conn: &mut Connection) -> Result<()> {
    unsafe {
        let _guard = LoadExtensionGuard::new(conn)?;
        conn.load_extension("./vec0.so", None)?;
    }
    Ok(())
}

// Insert embedding
pub async fn insert_embedding(
    db: &SqlitePool,
    node_id: i64,
    embedding: &[f32],
) -> Result<()> {
    let blob: Vec<u8> = embedding.iter()
        .flat_map(|f| f.to_le_bytes())
        .collect();

    sqlx::query(
        "INSERT INTO vec_nodes(rowid, embedding) VALUES (?, ?)"
    )
    .bind(node_id)
    .bind(&blob)
    .execute(db)
    .await?;

    Ok(())
}

// Vector similarity search
pub async fn find_similar(
    db: &SqlitePool,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<Node>> {
    let blob = serialize_vector(query_embedding);

    let results = sqlx::query_as::<_, Node>(
        "SELECT n.* FROM nodes n
         JOIN vec_nodes v ON n.id = v.rowid
         WHERE v.embedding MATCH ?
         ORDER BY distance
         LIMIT ?"
    )
    .bind(&blob)
    .bind(limit as i64)
    .fetch_all(db)
    .await?;

    Ok(results)
}
```

**Tests:**
- [x] Extension loads
- [x] Vector insertion works
- [x] Similarity search returns results
- [x] Shadow table pattern correct

**Time:** 1 day

---

#### Day 7: Hybrid Search with RRF (Spec V2)

**Implementation:**

```rust
// src/query/hybrid_search.rs - NEW FILE

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
        // Reciprocal Rank Fusion query
        let results = sqlx::query_as::<_, (Node, f32)>(
            r#"
            WITH
            fts_results AS (
                SELECT
                    rowid,
                    ROW_NUMBER() OVER (ORDER BY rank) AS fts_rank
                FROM nodes_fts
                WHERE nodes_fts MATCH ?
                LIMIT 50
            ),
            vec_results AS (
                SELECT
                    rowid,
                    ROW_NUMBER() OVER (ORDER BY distance) AS vec_rank
                FROM vec_nodes
                WHERE embedding MATCH ?
                ORDER BY distance
                LIMIT 50
            ),
            rrf_scores AS (
                SELECT
                    COALESCE(f.rowid, v.rowid) AS rowid,
                    COALESCE(1.0 / (60 + f.fts_rank), 0) +
                    COALESCE(1.0 / (60 + v.vec_rank), 0) AS rrf_score
                FROM fts_results f
                FULL OUTER JOIN vec_results v ON f.rowid = v.rowid
            )
            SELECT n.*, r.rrf_score
            FROM nodes n
            JOIN rrf_scores r ON n.id = r.rowid
            ORDER BY r.rrf_score DESC
            LIMIT ?
            "#
        )
        .bind(text_query)
        .bind(serialize_vector(vector_query))
        .bind(limit as i64)
        .fetch_all(&self.db)
        .await?;

        Ok(results)
    }
}
```

**Tests:**
- [x] FTS only returns keyword matches
- [x] Vector only returns semantic matches
- [x] Hybrid returns best of both
- [x] RRF scoring correct

**Time:** 1 day

---

### Phase 3: Visualization (Week 6-8) - 5 days

**Approach:** Tauri + Sigma.js + Apache Arrow IPC

#### Day 8-9: Tauri Desktop App

**Setup:**

```bash
cd userspace/synapse
cargo install tauri-cli
cargo tauri init
```

**src-tauri/main.rs:**

```rust
#[tauri::command]
async fn get_graph_neighborhood(node_id: String, hops: i32) -> Result<Vec<u8>, String> {
    let db = get_db().await;
    let query = QueryEngine::new(db);
    let (nodes, edges) = query.get_neighborhood(&node_id, hops).await
        .map_err(|e| e.to_string())?;

    // Serialize to Apache Arrow IPC
    let arrow_bytes = serialize_to_arrow(nodes, edges)?;

    Ok(arrow_bytes)
}

fn serialize_to_arrow(nodes: Vec<Node>, edges: Vec<Edge>) -> Result<Vec<u8>> {
    use arrow::array::*;
    use arrow::ipc::writer::StreamWriter;

    let schema = Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("label", DataType::Utf8, false),
        Field::new("x", DataType::Float32, true),
        Field::new("y", DataType::Float32, true),
    ]);

    let batch = RecordBatch::try_new(
        Arc::new(schema),
        vec![
            Arc::new(StringArray::from(nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>())),
            Arc::new(StringArray::from(nodes.iter().map(|n| n.get_property("name").unwrap_or_default().as_str().unwrap_or("")).collect::<Vec<_>>())),
            Arc::new(Float32Array::from(vec![0.0; nodes.len()])),  // TODO: Layout algorithm
            Arc::new(Float32Array::from(vec![0.0; nodes.len()])),
        ],
    )?;

    let mut buffer = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut buffer, &batch.schema())?;
        writer.write(&batch)?;
        writer.finish()?;
    }

    Ok(buffer)
}
```

**Frontend (Svelte):**

```svelte
<script>
import { invoke } from '@tauri-apps/api/tauri';
import { tableFromIPC } from 'apache-arrow';
import Sigma from 'sigma';
import Graph from 'graphology';

let sigmaInstance;

async function loadGraph() {
    const arrowBytes = await invoke('get_graph_neighborhood', {
        nodeId: 'root',
        hops: 2,
    });

    const table = tableFromIPC(arrowBytes);

    const graph = new Graph();
    for (let i = 0; i < table.numRows; i++) {
        graph.addNode(table.getChild('id').get(i), {
            label: table.getChild('label').get(i),
            x: table.getChild('x').get(i),
            y: table.getChild('y').get(i),
        });
    }

    const container = document.getElementById('sigma-container');
    sigmaInstance = new Sigma(graph, container);
}

onMount(() => loadGraph());
</script>

<div id="sigma-container" style="width: 100vw; height: 100vh;"></div>
```

**Cargo.toml:**
```toml
tauri = { version = "2.0", features = ["shell-open"] }
arrow = "51.0"
arrow-ipc = "51.0"
```

**Time:** 2 days

---

#### Day 10-12: Sigma.js Integration + Polish

- Force-directed layout
- Click node → expand neighborhood
- Search interface
- Node detail panel
- Export graph image

**Time:** 3 days

---

## Final Dependency List

```toml
[dependencies]
# Core
sqlx = { version = "0.7", features = ["runtime-tokio", "sqlite"] }
tokio = { version = "1.35", features = ["full"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
uuid = { version = "1.6", features = ["v4", "serde"] }
chrono = { version = "0.4", features = ["serde"] }
anyhow = "1.0"

# File watching (UPDATED)
notify-debouncer-full = "0.3"

# Hashing
sha2 = "0.10"

# Neural engine
ort = { version = "2.0", features = ["cuda", "download-binaries"] }
tokenizers = "0.15"
ndarray = "0.15"

# Graph algorithms
petgraph = "0.6"

# Desktop UI
tauri = { version = "2.0", features = ["shell-open"] }

# Data serialization
arrow = "51.0"
arrow-ipc = "51.0"

# SQLite vector extension (system library)
# rusqlite = { version = "0.31", features = ["load_extension"] }
```

**External Dependencies:**
- sqlite-vec extension (100KB)
- GLiNER quantized model (100MB)
- Tokenizer (5MB)

---

## Timeline Summary

| Phase | Days | Deliverables |
|-------|------|--------------|
| 1.5 - Critical Fixes | 3 | Portable, robust, optimized |
| 2 - Neural | 4 | Entity extraction, vector search |
| 3 - Visualization | 5 | Desktop app with graph UI |
| **Total** | **12 days** | **Full spec compliance** |

---

## Success Criteria

**Phase 1.5 Complete:**
- [ ] Database portable across machines
- [ ] Atomic saves handled (no .swp files)
- [ ] Unchanged files skipped (hash check)
- [ ] JSONB with functional indexes

**Phase 2 Complete:**
- [ ] GLiNER extracts entities from text
- [ ] Vector similarity search works
- [ ] Hybrid search combines FTS + vectors
- [ ] Confidence scores on all edges

**Phase 3 Complete:**
- [ ] Desktop app launches
- [ ] Graph renders 10,000+ nodes smoothly
- [ ] Click node → expand neighborhood
- [ ] Search finds nodes/entities

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| sqlite-vec compilation fails | Bundle precompiled .so/.dll |
| ONNX model export fails | Use community-exported models |
| GPU acceleration unavailable | CPU fallback (slower but works) |
| Tauri build issues | Fallback to CLI + web view |
| Breaking existing tests | Run tests after each phase |

---

## Next Steps

1. **Review this plan** - Ensure you understand all phases
2. **Set up environment** - Install dependencies
3. **Start Phase 1.5 Day 1** - Path normalization
4. **Run tests after each day**
5. **Move to next phase only when previous complete**

**DO NOT skip ahead. Each phase builds on the previous.**

---

## Documentation Updates Required

After implementation:
1. Update README.md with new features
2. Create ARCHITECTURE.md with diagrams
3. Document API in rustdoc
4. Create user guide for desktop app
5. Write developer guide for contributors

---

**This is your complete roadmap to a production-ready, spec-compliant Synapse.**

Start with Day 1. 🚀
