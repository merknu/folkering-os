# Synapse: Specification vs Implementation Comparison

**Date:** 2026-01-25

---

## Feature Comparison Matrix

| Feature | Spec Requirement | Current Implementation | Status | Gap Severity | Fix Priority |
|---------|------------------|------------------------|--------|--------------|--------------|
| **Storage Model** |
| Path Storage | Relative to project root | Absolute paths | ❌ | HIGH | P0 |
| Project Root | Stored in metadata table | Not tracked | ❌ | HIGH | P0 |
| Portability | Database movable with data | Breaks on move | ❌ | HIGH | P0 |
| **File Watching** |
| Debouncing | 1.0s settling period | Immediate events | ❌ | HIGH | P0 |
| Temp File Filter | Ignore .swp, .tmp, ~ | No filtering | ❌ | HIGH | P0 |
| Atomic Writes | Detect create→rename | Naive event handling | ❌ | HIGH | P0 |
| Ignore Patterns | .git, node_modules | No exclusions | ❌ | MEDIUM | P1 |
| Event Coalescing | Multiple events → single | Each event separate | ❌ | MEDIUM | P1 |
| **Database Schema** |
| Node Types | resources + entities | Single nodes table | ⚠️ | HIGH | P0 |
| Polymorphic Edges | source_type/target_type | Simple source_id/target_id | ⚠️ | HIGH | P0 |
| Content Hash | SHA-256 for change detection | Optional field, unused | ⚠️ | HIGH | P0 |
| Confidence Scores | From NER/embeddings | Not tracked | ❌ | MEDIUM | P1 |
| Session Persistence | Events in database | In-memory only | ⚠️ | MEDIUM | P1 |
| **Entity Extraction** |
| NER Engine | GLiNER via ONNX | Regex patterns | ❌ | HIGH | P1 |
| Entity Types | PERSON, ORG, LOC, DATE | Email, @mention only | ❌ | HIGH | P1 |
| Zero-Shot | Arbitrary labels at runtime | Fixed patterns | ❌ | HIGH | P1 |
| Confidence | Probability scores | Binary (match/no-match) | ❌ | MEDIUM | P1 |
| **Vector Embeddings** |
| Storage | sqlite-vec virtual table | External DB placeholder | ❌ | HIGH | P1 |
| Local-First | All embeddings in SQLite | Designed for Qdrant | ❌ | HIGH | P1 |
| Similarity Search | SQL MATCH query | Not implemented | ❌ | HIGH | P1 |
| Dimension | 384 (all-MiniLM-L6-v2) | Not specified | ❌ | LOW | P2 |
| **Graph Queries** |
| Recursive CTEs | WITH RECURSIVE | Basic recursion | ✅ | LOW | - |
| Cycle Detection | path_trace | None | ⚠️ | MEDIUM | P1 |
| Depth Limit | Configurable hops | Fixed depth | ✅ | LOW | - |
| Bidirectional | Source OR target | Separate queries | ⚠️ | LOW | P2 |
| **Concurrency** |
| Multi-Process | Separate indexer process | Single-threaded | ❌ | LOW | P2 |
| Event Queue | Async message passing | Immediate processing | ❌ | LOW | P2 |
| GIL Avoidance | CPU work in subprocess | N/A (Rust) | ✅ | - | - |
| **Visualization** |
| Graph Renderer | Sigma.js | None | ❌ | LOW | P2 |
| Desktop UI | PyQt6 or Tauri | CLI only | ❌ | LOW | P2 |
| Bridge Layer | Python↔JS communication | N/A | ❌ | LOW | P2 |
| **Testing** |
| Unit Tests | Comprehensive suite | Basic examples | ⚠️ | MEDIUM | P1 |
| Atomic Write Test | VSCode save simulation | Not tested | ❌ | HIGH | P0 |
| Portability Test | Move database | Not tested | ❌ | HIGH | P0 |
| Entity Extraction | NER accuracy metrics | Not tested | ❌ | MEDIUM | P1 |

**Legend:**
- ✅ Fully Implemented
- ⚠️ Partially Implemented
- ❌ Not Implemented

**Priority:**
- P0: Critical (blocks spec compliance)
- P1: High (core functionality)
- P2: Medium (nice-to-have)

---

## Detailed Gap Analysis

### 1. Storage Model: Portability ❌ CRITICAL GAP

**Spec Quote:**
> "Paths are stored relative to the database root rather than absolute paths. This relative addressing is crucial for portability; if a user moves a project folder containing a .tmsu database to a new drive, the tags remain valid because they reference files relative to the project root."

**Current Code:**
```rust
// src/graph/mod.rs:199
pub async fn register_path(&self, node_id: &str, path: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO file_paths (node_id, path) VALUES (?, ?)
         ON CONFLICT(node_id) DO UPDATE SET path = excluded.path"
    )
    .bind(node_id)
    .bind(path)  // ← ABSOLUTE PATH STORED
    .execute(&self.db)
    .await?;
    Ok(())
}
```

**Why This Breaks:**
```
# Initial state
/home/user/project/
├── synapse.db
└── documents/
    └── report.pdf

# Database stores: /home/user/project/documents/report.pdf

# User moves project
/mnt/backup/project/
├── synapse.db
└── documents/
    └── report.pdf

# Database still has: /home/user/project/documents/report.pdf
# → FILE NOT FOUND
```

**Fix Required:**
```rust
pub struct GraphDB {
    db: SqlitePool,
    project_root: PathBuf,  // NEW
}

pub async fn register_path(&self, node_id: &str, absolute_path: &str) -> Result<()> {
    let abs = PathBuf::from(absolute_path);
    let relative = abs.strip_prefix(&self.project_root)?
        .to_string_lossy();

    sqlx::query("INSERT INTO file_paths (node_id, path) VALUES (?, ?)")
        .bind(node_id)
        .bind(relative.as_ref())  // ← RELATIVE PATH
        .execute(&self.db)
        .await?;
    Ok(())
}
```

---

### 2. File Watching: Atomic Writes ❌ CRITICAL GAP

**Spec Quote:**
> "Modern text editors do not write directly to files. To prevent data loss during a crash, they employ an 'atomic save' strategy: Create Temp → Flush → Rename. To the operating system—and watchdog—this sequence appears as:
> - CREATED: .file.txt.swp
> - MODIFIED: .file.txt.swp (multiple times)
> - MOVED_FROM: .file.txt.swp → MOVED_TO: file.txt"

**Current Code:**
```rust
// src/observer/mod.rs:119
async fn handle_event(&self, event: Event) {
    match event.kind {
        EventKind::Modify(_) => {
            for path in event.paths {
                self.handle_file_edit(path).await;  // ← IMMEDIATE
            }
        }
        _ => {}
    }
}
```

**Why This Breaks:**
```
VSCode saves file.txt:
  1. CREATE file.txt.swp     → Observer indexes .swp file ❌
  2. MODIFY file.txt.swp     → Observer re-indexes .swp ❌
  3. MODIFY file.txt.swp     → Observer re-indexes again ❌
  4. RENAME .swp → file.txt  → Observer misses final file ❌

Result: .swp in database, file.txt not updated
```

**Fix Required:**
```rust
use std::collections::HashMap;
use std::time::{Duration, Instant};

const DEBOUNCE_INTERVAL: Duration = Duration::from_secs(1);
const IGNORE_EXTENSIONS: &[&str] = &[".swp", ".tmp", "~"];

pub struct DebouncedObserver {
    pending: HashMap<PathBuf, Instant>,
}

async fn handle_event(&mut self, event: Event) {
    for path in event.paths {
        // Filter temp files
        if self.is_ignored(&path) {
            continue;
        }

        // Update debounce timer
        self.pending.insert(path.clone(), Instant::now());

        // Spawn timer
        let pending = self.pending.clone();
        tokio::spawn(async move {
            sleep(DEBOUNCE_INTERVAL).await;

            // Check if still pending
            if let Some(last_time) = pending.get(&path) {
                if last_time.elapsed() >= DEBOUNCE_INTERVAL {
                    // File stabilized, process it
                    process_file(&path).await;
                }
            }
        });
    }
}
```

---

### 3. Database Schema: Polymorphism ⚠️ PARTIALLY IMPLEMENTED

**Spec Requirement:**
```sql
CREATE TABLE relationships (
    source_id INTEGER NOT NULL,
    source_type TEXT NOT NULL CHECK(source_type IN ('resource', 'entity')),
    target_id INTEGER NOT NULL,
    target_type TEXT NOT NULL CHECK(target_type IN ('resource', 'entity')),
    ...
);
```

**Current Schema:**
```sql
CREATE TABLE edges (
    source_id TEXT NOT NULL,  -- UUID string
    target_id TEXT NOT NULL,  -- UUID string
    type TEXT NOT NULL,       -- Edge type (CO_OCCURRED, etc.)
    ...
);
```

**Why This Matters:**
```
Current:
  Can't differentiate:
  - file → file (CO_OCCURRED)
  - file → person (EDITED_BY)
  - person → person (COLLABORATED_WITH)

  All edges are treated the same.

Spec-Compliant:
  relationships(
    source_id: 1, source_type: 'resource',   -- file
    target_id: 5, target_type: 'entity',     -- person
    predicate: 'EDITED_BY'
  )

  Enables queries like:
  "Find all people who edited this file"
  "Find all files Alice and Bob both worked on"
```

**Migration Required:**
```sql
-- Split nodes into resources + entities
CREATE TABLE resources AS
  SELECT * FROM nodes WHERE type = 'file';

CREATE TABLE entities AS
  SELECT * FROM nodes WHERE type IN ('person', 'app', 'tag');

-- Update edges to relationships
ALTER TABLE edges ADD COLUMN source_type TEXT DEFAULT 'resource';
ALTER TABLE edges ADD COLUMN target_type TEXT DEFAULT 'resource';

UPDATE edges SET
  source_type = (SELECT type FROM nodes WHERE id = edges.source_id),
  target_type = (SELECT type FROM nodes WHERE id = edges.target_id);
```

---

### 4. Entity Extraction: Neural Engine ❌ MISSING

**Spec Requirement:**
> "GLiNER (Generalist and Lightweight Named Entity Recognition) via ONNX Runtime. Zero-shot extraction of arbitrary entity types."

**Current Implementation:**
```rust
// src/observer/mod.rs:224
fn extract_entities(&self, text: &str) -> Vec<String> {
    let email_regex = regex::Regex::new(r"\b[\w._%+-]+@[\w.-]+\.[A-Z]{2,}\b").unwrap();
    let mention_regex = regex::Regex::new(r"@(\w+)").unwrap();

    let mut entities = Vec::new();
    for cap in email_regex.captures_iter(text) {
        entities.push(cap[0].to_string());
    }
    for cap in mention_regex.captures_iter(text) {
        entities.push(cap[1].to_string());
    }
    entities
}
```

**What This Misses:**
```
Input text:
"Alice from Microsoft met Bob in Seattle to discuss the Q4 2024 report."

Current extraction:
  - Nothing (no emails or @mentions)

GLiNER would extract:
  - "Alice" → PERSON (confidence: 0.95)
  - "Microsoft" → ORG (confidence: 0.98)
  - "Bob" → PERSON (confidence: 0.92)
  - "Seattle" → LOC (confidence: 0.89)
  - "Q4 2024" → DATE (confidence: 0.85)
```

**Required Architecture:**
```rust
// 1. Rust calls Python subprocess
pub struct GLiNERService {
    process: Child,
}

impl GLiNERService {
    pub async fn extract(&self, text: &str, labels: &[&str]) -> Result<Vec<Entity>> {
        // Send JSON to Python stdin
        let request = json!({"text": text, "labels": labels});
        self.stdin.write_all(request.to_string().as_bytes())?;

        // Read JSON from Python stdout
        let response: Vec<Entity> = serde_json::from_str(&self.stdout.read_line()?)?;
        Ok(response)
    }
}

// 2. Python runs ONNX
# neural_server.py
import onnxruntime as ort

session = ort.InferenceSession("gliner.onnx")

for line in sys.stdin:
    request = json.loads(line)
    outputs = session.run(None, tokenize(request['text']))
    entities = decode(outputs, request['labels'])
    print(json.dumps(entities))
```

---

### 5. Vector Embeddings: sqlite-vec ❌ MISSING

**Spec Requirement:**
```sql
CREATE VIRTUAL TABLE vec_items USING vec0(
    embedding float[384]
);

CREATE TABLE embedding_map (
    rowid INTEGER PRIMARY KEY,
    item_id INTEGER NOT NULL,
    item_type TEXT NOT NULL
);
```

**Current Schema:**
```sql
CREATE TABLE vector_embeddings (
    node_id TEXT PRIMARY KEY,
    vector_id TEXT NOT NULL,  -- External DB reference
    model TEXT NOT NULL
);
```

**Why This Violates Spec:**
- Spec: "Local-first. No cloud dependency."
- Current: Designed for external vector DB (Qdrant/Milvus)
- Current: No actual vector storage, just references

**What's Missing:**
```rust
// Can't do this query currently:
pub async fn find_similar(&self, file_id: &str) -> Result<Vec<Node>> {
    // 1. Get file's embedding
    let embedding = get_embedding(file_id).await?;

    // 2. Search for similar vectors
    let results = sqlx::query_as::<_, Node>(
        "SELECT * FROM vec_items
         WHERE embedding MATCH ?
         ORDER BY distance
         LIMIT 10"
    )
    .bind(serialize_vector(&embedding))
    .fetch_all(&self.db)
    .await?;

    Ok(results)
}
```

**Required:**
1. Compile sqlite-vec extension
2. Load extension in SQLite connection
3. Create virtual table
4. Serialize f32 vectors to bytes
5. Implement similarity search

---

## Priority Fixes Summary

### P0: Critical (Must Fix Before Production)

1. **Relative Path Storage** (1 day)
   - Prevents database portability
   - User expectation: "It just works when I move project"

2. **Debounced Observer** (1 day)
   - Prevents temp file indexing
   - Prevents multiple re-indexes per save

3. **Content Hashing** (0.5 day)
   - Prevents unnecessary re-indexing
   - Essential for performance

### P1: High (Core Functionality)

4. **Polymorphic Schema** (1 day)
   - Enables resource↔entity relationships
   - Required for entity extraction to be useful

5. **GLiNER Integration** (2 days)
   - Core feature of spec
   - Enables automatic relationship discovery

6. **sqlite-vec Integration** (1 day)
   - Local-first vector search
   - Enables "find similar documents"

### P2: Medium (Nice-to-Have)

7. **Visualization** (5 days)
   - User experience improvement
   - Not blocking core functionality

8. **Multi-Process** (2 days)
   - Performance optimization
   - Single-thread works for small datasets

---

## Compliance Score

**Current:** 40% (4/10 major features)

**After P0 Fixes:** 70% (7/10)

**After P1 Fixes:** 90% (9/10)

**After P2 Fixes:** 100% (Full compliance)

---

## Recommended Action Plan

**Week 3 (This Week):**
1. Read this document fully
2. Implement P0 fixes (3 days)
3. Run expanded test suite
4. Verify portability works

**Week 4:**
1. Implement P1 fixes (4 days)
2. Create entity extraction demo
3. Benchmark vector search performance

**Week 5+:**
1. Build visualization layer
2. User testing
3. Documentation

**Do not add new features until P0 complete.**
