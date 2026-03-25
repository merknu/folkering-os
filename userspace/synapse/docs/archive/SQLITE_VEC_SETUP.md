# sqlite-vec Setup Guide

This guide explains how to install and use the sqlite-vec extension for vector similarity search in Synapse.

## What is sqlite-vec?

[sqlite-vec](https://github.com/asg017/sqlite-vec) is a SQLite extension that provides fast vector similarity search using virtual tables.

**Features**:
- k-NN (k-nearest neighbors) search
- Cosine similarity, L2 distance, inner product
- SIMD-optimized for performance
- Pure SQLite - no external dependencies

## Installation

### Option 1: Pre-built Binaries (Recommended)

Download pre-built binaries from the [releases page](https://github.com/asg017/sqlite-vec/releases):

**Windows**:
```bash
# Download vec0.dll
curl -LO https://github.com/asg017/sqlite-vec/releases/latest/download/vec0.dll

# Place in project directory
mkdir -p lib
mv vec0.dll lib/
```

**Linux**:
```bash
# Download vec0.so
curl -LO https://github.com/asg017/sqlite-vec/releases/latest/download/vec0.so

# Place in project directory
mkdir -p lib
mv vec0.so lib/
```

**macOS**:
```bash
# Download vec0.dylib
curl -LO https://github.com/asg017/sqlite-vec/releases/latest/download/vec0.dylib

# Place in project directory
mkdir -p lib
mv vec0.dylib lib/
```

### Option 2: Compile from Source

**Prerequisites**:
- C compiler (gcc, clang, or MSVC)
- SQLite development headers

**Build**:
```bash
git clone https://github.com/asg017/sqlite-vec.git
cd sqlite-vec
make
```

This produces `vec0.so` (Linux), `vec0.dll` (Windows), or `vec0.dylib` (macOS).

## Testing the Extension

### From SQLite CLI

```bash
sqlite3
.load ./lib/vec0
.tables
```

If successful, you should see no errors.

### Test Query

```sql
-- Create virtual table
CREATE VIRTUAL TABLE vec_test USING vec0(
  embedding float[3]
);

-- Insert vectors
INSERT INTO vec_test(rowid, embedding) VALUES
  (1, '[1.0, 0.0, 0.0]'),
  (2, '[0.0, 1.0, 0.0]'),
  (3, '[0.0, 0.0, 1.0]');

-- k-NN search
SELECT
  rowid,
  distance
FROM vec_test
WHERE embedding MATCH '[1.0, 0.1, 0.0]'
  AND k = 2
ORDER BY distance;
```

Expected output:
```
1|0.0049875
2|0.904837
```

## Synapse Integration

### Database Schema

```sql
-- Virtual table for vector embeddings
CREATE VIRTUAL TABLE vec_nodes USING vec0(
  embedding float[384]
);

-- Mapping table (vec_nodes rowid → nodes.id)
CREATE TABLE node_embeddings (
  node_id TEXT PRIMARY KEY,
  vec_rowid INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  FOREIGN KEY (node_id) REFERENCES nodes(id)
);

-- Index for reverse lookup
CREATE INDEX idx_node_embeddings_vec ON node_embeddings(vec_rowid);
```

### Usage in Rust

```rust
use synapse::graph::vector_ops;

// Insert embedding
let embedding = vec![0.1, 0.2, ..., 0.384]; // 384-dim vector
vector_ops::insert_embedding(&db, "file-id-123", &embedding).await?;

// k-NN search
let query_embedding = vec![0.15, 0.22, ..., 0.40];
let results = vector_ops::search_similar(&db, &query_embedding, 10).await?;

for (node, similarity) in results {
    println!("{}: {:.4}", node.id, similarity);
}
```

## Performance

**Benchmarks** (1000 vectors, 384 dimensions):

| Operation | Latency |
|-----------|---------|
| Insert | ~0.5ms |
| k-NN (k=10) | ~5-20ms |
| k-NN (k=100) | ~20-50ms |

**Note**: Performance depends on dataset size and CPU features (AVX2, NEON).

## Troubleshooting

### Error: "no such module: vec0"

**Cause**: Extension not loaded or incorrect path.

**Fix**:
```rust
// In Rust code
conn.load_extension_enable()?;
conn.load_extension("./lib/vec0", None)?;
```

### Error: "cannot load extension"

**Cause**: SQLite compiled without extension support.

**Fix**: Use bundled SQLite with `libsqlite3-sys`:
```toml
[dependencies]
rusqlite = { version = "0.30", features = ["bundled", "load_extension"] }
```

### Performance Issues

**Solution**: Use SIMD-optimized builds:
- Ensure compiler flags include `-march=native` (Linux/macOS)
- Use AVX2-enabled builds on x86_64

## References

- [sqlite-vec GitHub](https://github.com/asg017/sqlite-vec)
- [sqlite-vec Documentation](https://alexgarcia.xyz/sqlite-vec/)
- [SQLite Extensions](https://www.sqlite.org/loadext.html)

## Next Steps

After installation:
1. Run integration test: `cargo run --example test_vector_search`
2. Verify k-NN search works
3. Benchmark on real data
