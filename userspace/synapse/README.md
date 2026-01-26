# Synapse - Graph Filesystem

**Status:** Phase 1 Implementation Complete ✅

Knowledge graph filesystem that replaces traditional hierarchical folders with a web of contextual relationships.

## Philosophy

Files should not just exist in a location - they exist in a web of context:
- Who edited them
- What apps opened them
- Which files were used together
- What entities they mention
- How similar they are semantically

## Architecture

```
┌─────────────────────────────────────────┐
│  Applications (read/write files)       │
└──────────────┬──────────────────────────┘
               │ File system events
               ▼
┌─────────────────────────────────────────┐
│  Observer Daemon (watches changes)      │
│  - Temporal co-occurrence heuristics    │
│  - NER for entity extraction            │
│  - Application context tracking         │
└──────────────┬──────────────────────────┘
               │ Creates edges
               ▼
┌─────────────────────────────────────────┐
│  Knowledge Graph (SQLite)               │
│  Nodes: files, people, apps, events... │
│  Edges: weighted relationships          │
└──────────────┬──────────────────────────┘
               │ Queries
               ▼
┌─────────────────────────────────────────┐
│  Query Engine (graph traversal)         │
│  - By tag, person, project             │
│  - By co-occurrence, similarity        │
│  - By time window                      │
└─────────────────────────────────────────┘
```

## Data Model

### Nodes (7 types)
- **File**: Documents, code, media
- **Person**: Authors, collaborators
- **App**: Applications that open files
- **Event**: Time-based activities
- **Tag**: User-defined labels
- **Project**: Organizational groups
- **Location**: Physical/virtual places

### Edges (12 types)
- `CONTAINS`: Folder contains file
- `EDITED_BY`: File edited by person (weight = edit frequency)
- `OPENED_WITH`: File opened with app
- `MENTIONS`: File mentions entity
- `SHARED_WITH`: Shared between people
- `HAPPENED_DURING`: Event in time window
- `CO_OCCURRED`: Files used together (weight = session count)
- `SIMILAR_TO`: Semantic similarity (weight = cosine distance)
- `DEPENDS_ON`: Code dependencies
- `REFERENCES`: Document citations
- `PARENT_OF`: Hierarchical relationships
- `TAGGED_WITH`: User tags

## Installation

```bash
cd userspace/synapse
cargo build --release
```

## Usage

### Start Observer Daemon

```bash
./target/release/synapse [db_path] [watch_path]
# Example:
./target/release/synapse synapse.db ~/Documents
```

The observer will:
1. Watch file system events
2. Track file accesses within sessions (5-minute windows)
3. Create CO_OCCURRED edges for files used together
4. Create EDITED_BY edges for modifications
5. Extract entities from filenames/content (Phase 1: simple regex)

### Query the Graph

```bash
./target/release/synapse-cli [db_path]
```

Available commands:
- `tag <tag_name>` - Find files with tag
- `edited <person>` - Files edited by person
- `cooccur <file_id>` - Files used together
- `similar <file_id>` - Semantically similar files
- `project <name>` - Files in project
- `today` - Files accessed today
- `search <query>` - Full-text search
- `neighborhood <id> [hops]` - Graph neighborhood (N hops)
- `stats` - Graph statistics

### Example Queries

**"What did I work on today?"**
```
synapse> today
Files accessed today:
  - report.pdf [uuid-123]
  - data.csv [uuid-456]
```

**"Find files I usually work on with report.pdf"**
```
synapse> cooccur uuid-123 0.5
Files co-occurring with 'uuid-123' (min weight 0.5):
  - data.csv [uuid-456]
  - analysis.py [uuid-789]
```

**"What files are related to 'machine learning' project?"**
```
synapse> project machine-learning
Files in project 'machine-learning':
  - model.py [uuid-111]
  - dataset.csv [uuid-222]
```

## Heuristics

### Temporal Co-occurrence (Phase 1)
Files accessed within 5-minute sessions = `CO_OCCURRED` edge
- Weight increases with frequency: 1 session = 0.3, 5+ sessions = 1.0
- Use case: "Show me files I usually work on together"

### Entity Extraction (Phase 1: Simple)
Uses regex patterns:
- Email addresses → MENTIONS person
- @mentions → MENTIONS person
- Phase 2 will use proper NER models

### Application Context
Tracks which apps open files → `OPENED_WITH` edges
- Use case: "Find all PDFs I've opened with Adobe"

### Edit Frequency
Counts edits per person → `EDITED_BY` edges
- Weight increases with edit count
- Use case: "Show me Alice's most-edited files"

## Phase 2 (Future)

- **Vector Embeddings**: Sentence-transformers for semantic similarity
- **External Vector DB**: Qdrant or Milvus for fast ANN search
- **Proper NER**: spaCy or Hugging Face models
- **Full-text Search**: Tantivy integration
- **Visualization**: Force-directed graph with D3.js

## Database Schema

See `migrations/001_initial_schema.sql` for complete schema.

Key tables:
- `nodes` - All entities (files, people, apps, etc.)
- `edges` - Weighted relationships (0.0-1.0)
- `sessions` - 5-minute working sessions
- `session_events` - File access log for co-occurrence
- `file_paths` - Legacy path mapping for compatibility

## API (Library Usage)

```rust
use synapse::{GraphDB, QueryEngine, Observer};
use sqlx::SqlitePool;

// Connect to database
let db = SqlitePool::connect("sqlite:synapse.db").await?;

// Create graph operations
let graph = GraphDB::new(db.clone());
let query = QueryEngine::new(db.clone());

// Create a node
let file_node = Node::new(
    NodeType::File,
    serde_json::json!({
        "name": "document.pdf",
        "size": 1024
    })
);
graph.create_node(&file_node).await?;

// Create an edge
let edge = Edge::new(
    file_id,
    person_id,
    EdgeType::EditedBy,
    0.8,
    None
);
graph.upsert_edge(&edge).await?;

// Query
let files = query.find_by_tag("work").await?;
let (nodes, edges) = query.get_neighborhood(&file_id, 2).await?;
```

## Performance

- **Storage**: SQLite (portable, single-file)
- **Indexing**: B-tree indexes on type, weight, timestamps
- **Queries**: Recursive CTEs for graph traversal (efficient for <1000 hops)
- **Observer**: Async event handling (tokio)

Expected performance:
- Query latency: <50ms for typical queries
- Observer overhead: Negligible (<1% CPU)
- Storage: ~1KB per node, ~200 bytes per edge

## Integration with Folkering OS

Synapse will replace the traditional VFS in Folkering:

1. **File API**: Applications call `open("/work/report.pdf")`
2. **Path Translation**: VFS translates path → node lookup
3. **Context Enrichment**: Observer creates edges based on access
4. **Intent Integration**: Intent Bus uses graph for semantic routing

Example: User says "Open that PDF I was working on with Alice"
→ Intent Bus queries: `find_edited_by("Alice") + find_by_timeframe(recent)`
→ Returns PDF with highest weight

## Testing

```bash
# Run tests
cargo test

# Run with example data
cargo run --example populate_graph

# Query the example graph
cargo run --bin synapse-cli
```

## Contributing

Current status: Solo developer (merkn)

Future work:
- Phase 2: Vector embeddings
- Phase 3: Real-time collaboration tracking
- Phase 4: Federated graphs (cross-device sync)

## License

Part of the Folkering OS project.

---

*"Files are not just data - they're nodes in your knowledge graph."* 🧠
