//! Test content hashing for change detection
//!
//! This test verifies that:
//! 1. Files with unchanged content are not re-indexed
//! 2. Files with changed content are re-indexed
//! 3. Hash computation is fast and reliable

use synapse::{GraphDB, Node, NodeType};
use synapse::graph::compute_file_hash;
use sqlx::SqlitePool;
use serde_json::json;
use anyhow::Result;
use std::fs;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Synapse Content Hashing Test ===\n");

    // Create temp directory
    let test_dir = std::env::current_dir()?.join("test_hashing");
    if test_dir.exists() {
        fs::remove_dir_all(&test_dir)?;
    }
    fs::create_dir_all(&test_dir)?;

    println!("📁 Test directory: {}\n", test_dir.display());

    // Create database
    let db_path = test_dir.join("test.db");
    let db = SqlitePool::connect(&format!("sqlite:{}?mode=rwc", db_path.display())).await?;

    // Run migrations
    println!("🔧 Running migrations...");
    run_migrations(&db).await?;
    println!("✅ Migrations complete\n");

    let graph = GraphDB::with_project_root(db.clone(), test_dir.clone());

    // ========================================================================
    // Test 1: Hash Computation
    // ========================================================================

    println!("Test 1: Hash Computation");

    // Create test file
    let file1_path = test_dir.join("test1.txt");
    fs::write(&file1_path, b"This is test content version 1.")?;

    // Compute hash
    let hash1 = compute_file_hash(&file1_path)?;
    println!("  File: test1.txt");
    println!("  Hash: {}", hash1);
    println!("  Length: {} chars", hash1.len());

    assert_eq!(hash1.len(), 64, "SHA-256 should be 64 hex characters");

    // Verify deterministic
    let hash1_again = compute_file_hash(&file1_path)?;
    assert_eq!(hash1, hash1_again, "Hash should be deterministic");

    println!("✅ Test 1 passed: Hash computation works\n");

    // ========================================================================
    // Test 2: Hash Changes with Content
    // ========================================================================

    println!("Test 2: Hash Changes with Content");

    // Modify file
    fs::write(&file1_path, b"This is test content version 2.")?;
    let hash2 = compute_file_hash(&file1_path)?;

    println!("  Original hash: {}", &hash1[..16]);
    println!("  Modified hash: {}", &hash2[..16]);

    assert_ne!(hash1, hash2, "Different content should have different hash");

    println!("✅ Test 2 passed: Hash changes with content\n");

    // ========================================================================
    // Test 3: Store and Retrieve Hash
    // ========================================================================

    println!("Test 3: Store and Retrieve Hash");

    // Create node
    let node1 = Node::new(NodeType::File, json!({"name": "test1.txt"}));
    let node_id = node1.id.clone();
    graph.create_node(&node1).await?;
    graph.register_path(&node_id, &file1_path.to_string_lossy()).await?;

    // Store hash
    graph.update_file_hash(&node_id, &hash2).await?;
    println!("  Stored hash for node {}", &node_id[..8]);

    // Retrieve hash
    let retrieved_hash = graph.get_file_hash(&node_id).await?;
    assert_eq!(retrieved_hash, Some(hash2.clone()), "Retrieved hash should match stored");

    println!("✅ Test 3 passed: Hash storage works\n");

    // ========================================================================
    // Test 4: Skip Re-indexing for Unchanged Files
    // ========================================================================

    println!("Test 4: Skip Re-indexing for Unchanged Files");

    // File content hasn't changed, hash should match
    let needs_reindex = graph.needs_reindexing(&node_id, &file1_path).await?;
    println!("  File unchanged, needs re-index: {}", needs_reindex);
    assert!(!needs_reindex, "Unchanged file should not need re-indexing");

    // Modify file
    fs::write(&file1_path, b"This is test content version 3.")?;

    let needs_reindex_after = graph.needs_reindexing(&node_id, &file1_path).await?;
    println!("  File modified, needs re-index: {}", needs_reindex_after);
    assert!(needs_reindex_after, "Modified file should need re-indexing");

    println!("✅ Test 4 passed: Re-indexing detection works\n");

    // ========================================================================
    // Test 5: Touch File (mtime changes, content unchanged)
    // ========================================================================

    println!("Test 5: Touch File (mtime changes, content unchanged)");

    // Write same content as version 3
    fs::write(&file1_path, b"This is test content version 3.")?;

    // Update hash
    let hash3 = compute_file_hash(&file1_path)?;
    graph.update_file_hash(&node_id, &hash3).await?;

    // "Touch" file by writing same content (simulates touch command)
    std::thread::sleep(std::time::Duration::from_millis(100));
    fs::write(&file1_path, b"This is test content version 3.")?;

    // mtime changed, but content (and hash) same
    let needs_reindex_touch = graph.needs_reindexing(&node_id, &file1_path).await?;
    println!("  File touched (mtime changed), needs re-index: {}", needs_reindex_touch);
    assert!(!needs_reindex_touch, "Touched file with same content should not need re-indexing");

    println!("✅ Test 5 passed: Touch detection works (mtime ignored)\n");

    // ========================================================================
    // Test 6: Index with Hash Tracking
    // ========================================================================

    println!("Test 6: Index with Hash Tracking");

    // Create another file
    let file2_path = test_dir.join("test2.txt");
    fs::write(&file2_path, b"Second test file.")?;

    let node2 = Node::new(NodeType::File, json!({"name": "test2.txt"}));
    let node2_id = node2.id.clone();
    graph.create_node(&node2).await?;
    graph.register_path(&node2_id, &file2_path.to_string_lossy()).await?;

    // First index (no hash stored)
    let (indexed1, reason1) = graph.index_file_with_hash(&node2_id, &file2_path).await?;
    println!("  First index: indexed={}, reason={}", indexed1, reason1);
    assert!(indexed1, "First index should happen");
    assert_eq!(reason1, "indexed");

    // Second index immediately (hash unchanged)
    let (indexed2, reason2) = graph.index_file_with_hash(&node2_id, &file2_path).await?;
    println!("  Second index: indexed={}, reason={}", indexed2, reason2);
    assert!(!indexed2, "Second index should be skipped");
    assert_eq!(reason2, "unchanged");

    // Modify and index again
    fs::write(&file2_path, b"Second test file MODIFIED.")?;
    let (indexed3, reason3) = graph.index_file_with_hash(&node2_id, &file2_path).await?;
    println!("  After modification: indexed={}, reason={}", indexed3, reason3);
    assert!(indexed3, "Modified file should be re-indexed");
    assert_eq!(reason3, "indexed");

    println!("✅ Test 6 passed: Index with hash tracking works\n");

    // ========================================================================
    // Test 7: Performance (Large File)
    // ========================================================================

    println!("Test 7: Performance (Large File)");

    // Create 10MB file
    let large_file_path = test_dir.join("large.bin");
    let chunk = vec![0xAB; 1024 * 1024]; // 1MB
    let mut file = fs::File::create(&large_file_path)?;
    use std::io::Write;
    for _ in 0..10 {
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    drop(file);

    println!("  Created 10MB test file");

    // Time hash computation
    let start = std::time::Instant::now();
    let large_hash = compute_file_hash(&large_file_path)?;
    let elapsed = start.elapsed();

    println!("  Hash: {}...", &large_hash[..16]);
    println!("  Time: {:?}", elapsed);
    assert!(elapsed.as_millis() < 500, "10MB file should hash in <500ms");

    println!("✅ Test 7 passed: Large file hashing is fast\n");

    // ========================================================================
    // Test 8: Get Stale Files
    // ========================================================================

    println!("Test 8: Get Stale Files");

    // Create file without hash
    let file3_path = test_dir.join("test3.txt");
    fs::write(&file3_path, b"Third file, no hash yet.")?;

    let node3 = Node::new(NodeType::File, json!({"name": "test3.txt"}));
    let node3_id = node3.id.clone();
    graph.create_node(&node3).await?;
    graph.register_path(&node3_id, &file3_path.to_string_lossy()).await?;

    // Get stale files
    let stale = graph.get_stale_files().await?;
    println!("  Found {} stale file(s)", stale.len());
    assert!(stale.len() >= 1, "Should find at least 1 stale file");

    let has_test3 = stale.iter().any(|(id, _)| id == &node3_id);
    assert!(has_test3, "test3.txt should be in stale list");

    println!("✅ Test 8 passed: Stale file detection works\n");

    // ========================================================================
    // Cleanup
    // ========================================================================

    db.close().await;
    // fs::remove_dir_all(&test_dir)?;

    println!("=== All Content Hashing Tests Passed! ===\n");
    println!("Key Results:");
    println!("✅ SHA-256 hash computation works");
    println!("✅ Hash changes with content");
    println!("✅ Hash storage and retrieval works");
    println!("✅ Re-indexing detection works");
    println!("✅ Touch detection works (mtime ignored)");
    println!("✅ Index with hash tracking works");
    println!("✅ Large file hashing is fast (<500ms for 10MB)");
    println!("✅ Stale file detection works");
    println!("\nPerformance Impact:");
    println!("- Unchanged files: 0 index operations (100% skip rate)");
    println!("- Touched files: 0 index operations (mtime ignored)");
    println!("- Modified files: 1 index operation (as needed)");

    Ok(())
}

async fn run_migrations(db: &SqlitePool) -> Result<()> {
    // Nodes
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS nodes (
            id TEXT PRIMARY KEY NOT NULL,
            type TEXT NOT NULL,
            properties TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            CHECK (type IN ('file', 'person', 'app', 'event', 'tag', 'project', 'location'))
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(type)").execute(db).await?;

    // Project meta
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS project_meta (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
    "#).execute(db).await?;

    // File paths
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS file_paths (
            node_id TEXT PRIMARY KEY NOT NULL,
            path TEXT NOT NULL UNIQUE,
            content_hash TEXT,
            last_indexed TEXT,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_paths_path ON file_paths(path)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_file_paths_hash ON file_paths(content_hash)").execute(db).await?;

    Ok(())
}
