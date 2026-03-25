# Synapse Phase 2: Neural Intelligence - Implementation Plan

**Date Created**: 2026-01-25
**Prerequisites**: Phase 1.5 Complete ✅ (80% spec compliance)
**Goal**: Achieve 95% specification compliance with neural intelligence features
**Estimated Duration**: 8-10 days

---

## Executive Summary

Phase 2 transforms Synapse from a robust graph filesystem into an **intelligent knowledge system** by adding:

1. **GLiNER Entity Extraction** - Real named entity recognition (not regex)
2. **Vector Search** - Semantic similarity via sqlite-vec
3. **Polymorphic Schema** - Resource↔Entity relationships
4. **Hybrid Search** - Combined keyword + semantic search

**Expected Outcome**: "Find similar documents" and "Which files mention Alice?" queries work.

---

## Phase 1.5 Achievements (Building On)

Before starting Phase 2, we have:

- ✅ **Database Portability** - Relative paths, cross-platform
- ✅ **Robust File Watching** - Debounced, atomic write detection
- ✅ **Intelligent Change Detection** - SHA-256 content hashing
- ✅ **Temporal Intelligence** - Session tracking, "what did I work on today?"
- ✅ **42/42 Tests Passing** - Production-ready code quality

**This is our solid foundation.**

---

## Phase 2 Scope

### In Scope

1. ✅ GLiNER entity extraction (ONNX Runtime)
2. ✅ sqlite-vec integration (vector search)
3. ✅ Polymorphic schema updates
4. ✅ Hybrid search (RRF algorithm)
5. ✅ Embedding generation pipeline
6. ⚠️ JSONB conversion (optional optimization)

### Out of Scope (Phase 3)

- ❌ Graph visualization (Tauri UI)
- ❌ Arrow IPC serialization (performance optimization)
- ❌ GPU acceleration (nice-to-have)
- ❌ Multi-process architecture

---

## Implementation Breakdown (8 Days)

### Day 1: GLiNER Model Preparation & ONNX Integration

**Goal**: Export GLiNER model to ONNX and verify inference works in Rust

**Tasks**:
1. Export GLiNER to ONNX (Python script)
2. Quantize to Int8 (400MB → 100MB)
3. Add `ort` crate dependency
4. Implement basic inference pipeline
5. Test entity extraction on sample text

**Deliverables**:
- `scripts/export_gliner.py` - Model export script
- `assets/models/gliner_quantized.onnx` - Quantized model (100MB)
- `src/neural/gliner.rs` - GLiNER service
- Test: Extract entities from "Alice and Bob discussed physics"

**Success Criteria**:
```rust
let gliner = GLiNERService::new("assets/models/gliner_quantized.onnx")?;
let entities = gliner.extract_entities(
    "Alice and Bob discussed physics",
    &["person", "concept"],
    0.5
)?;

assert_eq!(entities.len(), 3);
// ["Alice" (person), "Bob" (person), "physics" (concept)]
```

**Estimated Effort**: 1 day (8 hours)

---

### Day 2: Entity Node Creation & Storage

**Goal**: Create entity nodes from extracted text and link to resources

**Tasks**:
1. Update schema to support entity nodes
2. Implement entity deduplication logic
3. Create `REFERENCES` edges between resources and entities
4. Add entity metadata (confidence, source file)
5. Test full pipeline: file → extract → store entities

**Deliverables**:
- `migrations/006_entity_nodes.sql` - Schema updates
- `src/graph/entity_ops.rs` - Entity CRUD operations
- `src/ingestion/entity_pipeline.rs` - Full extraction pipeline
- Test: Index file, verify entities created

**Success Criteria**:
```rust
// Index a file containing "Alice works on Project X"
graph.index_file("docs/team.md").await?;

// Verify entities created
let alice = graph.find_entity_by_text("Alice").await?;
assert_eq!(alice.label, "person");

let project_x = graph.find_entity_by_text("Project X").await?;
assert_eq!(project_x.label, "project");

// Verify edges
let refs = graph.get_references("team.md").await?;
assert!(refs.contains(&alice.id));
assert!(refs.contains(&project_x.id));
```

**Estimated Effort**: 1 day (8 hours)

---

### Day 3: Embedding Generation Pipeline

**Goal**: Generate embeddings for files using sentence-transformers

**Tasks**:
1. Choose embedding model (all-MiniLM-L6-v2, 384 dims)
2. Export model to ONNX
3. Implement embedding generation in Rust
4. Add batching for efficiency
5. Test embedding similarity

**Deliverables**:
- `scripts/export_embeddings.py` - Model export
- `assets/models/minilm_l6_v2.onnx` - Embedding model (~80MB)
- `src/neural/embeddings.rs` - Embedding service
- Test: Generate embeddings, verify dimensions

**Success Criteria**:
```rust
let embedder = EmbeddingService::new("assets/models/minilm_l6_v2.onnx")?;

let text = "Machine learning with neural networks";
let embedding = embedder.generate(text)?;

assert_eq!(embedding.len(), 384);
assert!(embedding.iter().all(|&x| x.is_finite()));

// Similarity test
let similar = embedder.generate("Deep learning and AI")?;
let dissimilar = embedder.generate("Cooking recipes for pasta")?;

let sim_score = cosine_similarity(&embedding, &similar);
let dissim_score = cosine_similarity(&embedding, &dissimilar);

assert!(sim_score > 0.7);
assert!(dissim_score < 0.3);
```

**Estimated Effort**: 1 day (8 hours)

---

### Day 4: sqlite-vec Integration

**Goal**: Store and query vector embeddings using sqlite-vec

**Tasks**:
1. Compile sqlite-vec extension (or use pre-built)
2. Load extension in SQLite connection
3. Create virtual table for embeddings
4. Implement shadow table pattern
5. Test vector insertion and k-NN search

**Deliverables**:
- `migrations/007_vector_search.sql` - Virtual table setup
- `src/graph/vec_extension.rs` - Extension loading
- `src/graph/vector_ops.rs` - Vector CRUD operations
- Test: Insert vectors, k-NN search

**Success Criteria**:
```rust
// Insert embeddings
graph.insert_embedding(&file1_id, &embedding1).await?;
graph.insert_embedding(&file2_id, &embedding2).await?;

// Vector search (k-NN)
let similar_files = graph.vector_search(&query_embedding, 10).await?;

assert!(similar_files.len() <= 10);
assert!(similar_files[0].distance < 0.5);  // Close match
```

**Known Challenges**:
- sqlite-vec requires C compilation
- May need to bundle pre-compiled binaries
- sqlx doesn't support load_extension natively (workaround needed)

**Mitigation**:
- Provide build script for sqlite-vec
- Test on Windows, Linux, macOS
- Document installation steps

**Estimated Effort**: 1 day (8 hours)

---

### Day 5: Full Indexing Pipeline Integration

**Goal**: Connect all pieces - file change → embedding + entities → searchable

**Tasks**:
1. Update observer to trigger entity extraction
2. Generate embeddings on file save
3. Store entities and vectors atomically
4. Add incremental update logic (skip if content unchanged)
5. Test full pipeline end-to-end

**Deliverables**:
- `src/observer/neural_pipeline.rs` - Integrated pipeline
- Updated `src/observer/mod.rs` - Calls neural pipeline
- Test: Save file, verify entities + embeddings created

**Success Criteria**:
```rust
// Setup observer
let observer = Observer::with_neural_pipeline(db.clone()).await?;

// Create and save file
let file = "Alice and Bob are working on Project Mars";
fs::write("docs/team.md", file)?;

// Wait for indexing
tokio::time::sleep(Duration::from_secs(2)).await;

// Verify entities extracted
let entities = graph.get_entities_for_file("docs/team.md").await?;
assert!(entities.iter().any(|e| e.text == "Alice"));
assert!(entities.iter().any(|e| e.text == "Bob"));
assert!(entities.iter().any(|e| e.text == "Project Mars"));

// Verify embedding stored
let embedding = graph.get_embedding_for_file("docs/team.md").await?;
assert!(embedding.is_some());
```

**Estimated Effort**: 1 day (8 hours)

---

### Day 6: Hybrid Search (RRF Algorithm)

**Goal**: Implement Reciprocal Rank Fusion for combined keyword + vector search

**Tasks**:
1. Implement FTS (full-text search) using SQLite FTS5
2. Implement vector search (from Day 4)
3. Combine results using RRF algorithm
4. Add query rewriting (expand acronyms, synonyms)
5. Test search quality

**Deliverables**:
- `migrations/008_fts5.sql` - FTS5 virtual table
- `src/query/hybrid_search.rs` - RRF implementation
- Test: Search for "ML papers", verify results

**Success Criteria**:
```rust
let query = "machine learning research papers";

// Keyword search only
let fts_results = graph.search_fts(query, 10).await?;

// Vector search only
let query_embedding = embedder.generate(query)?;
let vec_results = graph.vector_search(&query_embedding, 10).await?;

// Hybrid search (RRF)
let hybrid_results = graph.hybrid_search(query, 10).await?;

// Hybrid should have best of both
assert!(hybrid_results.len() <= 10);
assert!(hybrid_results[0].rrf_score > 0.5);

// Contains results from both keyword and semantic
// (Some files may match keywords, others semantically similar)
```

**RRF Formula**:
```
Score = 1/(k + Rank_FTS) + 1/(k + Rank_Vector)
k = 60 (smoothing constant)
```

**Estimated Effort**: 1 day (8 hours)

---

### Day 7: Semantic Query Methods

**Goal**: High-level query API for common use cases

**Tasks**:
1. Implement "find similar documents" query
2. Implement "which files mention X?" query
3. Implement "files about concept Y" query
4. Add confidence scoring
5. Test with real-world examples

**Deliverables**:
- `src/query/semantic.rs` - Semantic query methods
- Updated `src/query/mod.rs` - Exports new API
- Test: Run all semantic queries

**Success Criteria**:
```rust
// Find similar documents
let similar = query.find_similar("design_doc.md", 0.7).await?;
assert!(similar.len() > 0);

// Files mentioning a person
let files = query.find_files_mentioning_entity("Alice", "person").await?;
assert!(files.iter().any(|f| f.name.contains("team")));

// Files about a concept
let ml_files = query.find_files_about("machine learning").await?;
assert!(ml_files.len() > 0);

// Confidence scoring
for file in ml_files {
    assert!(file.confidence >= 0.7);
}
```

**Estimated Effort**: 1 day (8 hours)

---

### Day 8: Testing, Documentation & Benchmarking

**Goal**: Comprehensive tests, performance benchmarks, and documentation

**Tasks**:
1. Write integration tests for all Phase 2 features
2. Benchmark entity extraction speed
3. Benchmark vector search latency
4. Document API with examples
5. Update README and completion docs

**Deliverables**:
- `examples/test_entity_extraction.rs` - Entity tests
- `examples/test_vector_search.rs` - Vector search tests
- `examples/test_hybrid_search.rs` - Hybrid search tests
- `examples/benchmark_neural.rs` - Performance benchmarks
- `PHASE_2_COMPLETE.md` - Completion documentation

**Success Criteria**:
- All tests passing (estimate: 60+ tests total)
- Entity extraction: <500ms per file (1-2 KB)
- Vector search: <50ms for k=10
- Hybrid search: <100ms for k=10
- Documentation complete with code examples

**Estimated Effort**: 1 day (8 hours)

---

## Total Timeline

| Day | Focus | Deliverable |
|-----|-------|-------------|
| 1 | GLiNER ONNX | Entity extraction working |
| 2 | Entity Storage | Entities linked to files |
| 3 | Embeddings | Vector generation working |
| 4 | sqlite-vec | Vector search working |
| 5 | Integration | Full pipeline end-to-end |
| 6 | Hybrid Search | RRF algorithm working |
| 7 | Semantic API | High-level queries working |
| 8 | Testing & Docs | All tests passing, documented |

**Total: 8 days (64 hours of focused work)**

**Buffer: +2 days for unexpected issues = 10 days total**

---

## Dependencies & Prerequisites

### Rust Crates

```toml
[dependencies]
# Existing (from Phase 1.5)
sqlx = { version = "0.7", features = ["runtime-tokio", "sqlite"] }
tokio = { version = "1.35", features = ["full"] }
notify = "6.1"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
sha2 = "0.10"

# New for Phase 2
ort = { version = "2.0", features = ["download-binaries"] }
tokenizers = "0.15"
ndarray = "0.15"

# Optional: Better debouncer (can keep custom for now)
# notify-debouncer-full = "0.3"
```

### External Dependencies

1. **sqlite-vec** (C extension)
   - Download: https://github.com/asg017/sqlite-vec/releases
   - Or compile: `gcc -shared -o vec0.so vec0.c`
   - Place in: System SQLite extensions dir or project `lib/`

2. **GLiNER Model** (ONNX)
   - Export from Python: `gliner_small-v2.1` → ONNX → Quantize
   - Size: ~100MB (quantized Int8)
   - Place in: `assets/models/gliner_quantized.onnx`

3. **Embedding Model** (ONNX)
   - Model: `sentence-transformers/all-MiniLM-L6-v2`
   - Size: ~80MB
   - Place in: `assets/models/minilm_l6_v2.onnx`

### Python Environment (for model export)

```bash
pip install gliner optimum onnxruntime sentence-transformers
```

---

## Testing Strategy

### Unit Tests

- Entity extraction accuracy
- Embedding generation correctness
- Vector similarity calculations
- RRF score computation

### Integration Tests

- End-to-end file indexing
- Search result quality
- Entity deduplication
- Incremental updates

### Benchmarks

- Entity extraction: 100 files
- Vector search: 10,000 embeddings
- Hybrid search: Mixed workload

### Real-World Validation

- Index actual project (Folkering OS codebase)
- Test queries: "Find files about IPC", "Who is mentioned most?"
- Measure accuracy and relevance

---

## Success Metrics

### Quantitative

- ✅ **Spec Compliance**: 80% → 95%
- ✅ **Entity Extraction**: Precision >80%, Recall >70%
- ✅ **Vector Search**: Top-10 accuracy >85%
- ✅ **Hybrid Search**: Better than FTS-only or vector-only
- ✅ **Performance**: <500ms end-to-end indexing per file

### Qualitative

- ✅ "Find similar documents" returns relevant results
- ✅ "Which files mention Alice?" returns correct files
- ✅ Entities are meaningful (not noise)
- ✅ Search feels "intelligent" (semantic understanding)

---

## Known Risks & Mitigation

### Risk 1: sqlite-vec Compilation Issues

**Risk**: C compilation fails on Windows/macOS
**Mitigation**: Provide pre-compiled binaries for common platforms
**Fallback**: Pure Rust vector search (slower but works)

### Risk 2: ONNX Runtime Complexity

**Risk**: ort crate difficult to configure (CUDA, binaries)
**Mitigation**: Use `download-binaries` feature, CPU-only first
**Fallback**: Python subprocess for inference (slower but reliable)

### Risk 3: Model Size & Performance

**Risk**: 100MB+ models too large, slow on CPU
**Mitigation**: Quantization (Int8), batching, caching
**Fallback**: Smaller models (gliner_nano, 30MB)

### Risk 4: Search Quality Below Expectations

**Risk**: Entities extracted are noisy, search irrelevant
**Mitigation**: Tune confidence thresholds, filter by entity type
**Iteration**: Collect feedback, retrain or adjust prompts

---

## Phase 2 vs Phase 1.5 Comparison

| Aspect | Phase 1.5 | Phase 2 |
|--------|-----------|---------|
| **Focus** | Robustness | Intelligence |
| **Key Feature** | Content hashing | Entity extraction |
| **Queries** | Temporal ("today") | Semantic ("similar") |
| **Dependencies** | SHA-256, notify | ONNX, sqlite-vec |
| **Complexity** | Medium | High |
| **Test Count** | 42 | ~60 (estimated) |
| **Spec Compliance** | 80% | 95% |

---

## Next Steps After Phase 2

### Phase 3: Visualization & UI (Future)

- Tauri desktop app
- Graph rendering (Sigma.js, Cosmograph)
- Interactive entity exploration
- Timeline view of sessions

### Phase 4: Advanced Features (Future)

- Multi-user support
- Real-time collaboration
- Federated knowledge graphs
- AI-assisted knowledge synthesis

---

## Decision Log (Phase 2 Specific)

### D017: Python Subprocess vs Native Rust ONNX

**Decision**: Start with native Rust (`ort` crate)
**Rationale**: Lower latency, better integration
**Fallback**: Python subprocess if ort proves difficult

### D018: Embedding Model Selection

**Decision**: all-MiniLM-L6-v2 (384 dims)
**Rationale**: Good quality, small size, fast
**Alternative**: all-mpnet-base-v2 (768 dims, higher quality but slower)

### D019: Entity Deduplication Strategy

**Decision**: Case-insensitive exact match on normalized text
**Rationale**: Simple, fast, works for 90% of cases
**Future**: Fuzzy matching, entity resolution

### D020: JSONB Conversion (Optional)

**Decision**: Defer to Phase 2.5 or 3
**Rationale**: JSON TEXT works fine for now, JSONB is optimization
**Benefit**: Functional indexes (can add later if needed)

---

## Resource Requirements

### Disk Space

- Models: ~200MB (GLiNER + Embeddings)
- Database growth: ~500MB for 10,000 files with embeddings
- Test data: ~100MB

**Total: ~800MB additional storage**

### Memory

- ONNX Runtime: ~500MB during inference
- Vector index: ~15MB per 10,000 embeddings (384 dims, float32)
- Database connections: ~50MB

**Total: ~600MB peak memory usage**

### Compute

- Entity extraction: CPU-bound (benefits from multi-core)
- Embedding generation: CPU-bound (can use GPU if available)
- Vector search: Memory-bound (fast with proper indexes)

**Recommended**: 4+ CPU cores, 8GB+ RAM

---

## Conclusion

Phase 2 transforms Synapse from a robust graph filesystem into an **intelligent knowledge system**. By adding GLiNER entity extraction, vector search, and hybrid search, users can:

- Find similar documents semantically
- Discover which files mention specific people/projects
- Search by concept, not just keywords
- Understand relationships between files and entities

**This is the neural intelligence layer that makes Synapse truly AI-powered.**

**Ready to start Day 1** 🚀

---

**Status**: Planning Complete
**Next**: Day 1 - GLiNER Model Preparation & ONNX Integration
**Estimated Completion**: 2026-02-02 (8-10 days from start)

**Tags**: #synapse #phase-2 #neural-intelligence #planning
