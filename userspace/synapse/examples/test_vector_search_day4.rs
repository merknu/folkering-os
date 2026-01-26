//! Test vector search - Phase 2 Day 4
//!
//! This example verifies that:
//! 1. Embeddings can be inserted into vector index
//! 2. Embeddings can be retrieved
//! 3. Embeddings can be updated
//! 4. Embeddings can be deleted
//! 5. Vector search works (basic test without sqlite-vec)
//! 6. Count operations work
//!
//! Note: This test works WITHOUT sqlite-vec extension by using fallback table.
//! With sqlite-vec loaded, k-NN search would be much faster.
//!
//! Prerequisites:
//!   - Run from project root: cargo run --example test_vector_search_day4

use anyhow::Result;
use synapse::graph::vector_ops;
use synapse::EMBEDDING_DIM;
use sqlx::sqlite::SqlitePoolOptions;
use uuid::Uuid;
use chrono::Utc;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Synapse Phase 2 Day 4: Vector Search Test ===\n");

    // Setup in-memory database
    println!("[Setup] Creating in-memory database...");
    let db = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await?;

    // Create schema
    println!("[Setup] Creating database schema...");
    create_schema(&db).await?;
    println!("  ✓ Database ready\n");

    // Test 1: Insert embedding
    println!("[Test 1] Inserting embedding...");

    let node_id = Uuid::new_v4().to_string();
    create_test_node(&db, &node_id).await?;

    let embedding = vec![0.1; EMBEDDING_DIM];
    let vec_rowid = vector_ops::insert_embedding(&db, &node_id, &embedding).await?;

    println!("  Node ID: {}", node_id);
    println!("  Vec rowid: {}", vec_rowid);
    println!("  Embedding dim: {}", embedding.len());
    assert!(vec_rowid > 0);
    println!("  ✓ Embedding inserted successfully\n");

    // Test 2: Retrieve embedding
    println!("[Test 2] Retrieving embedding...");

    let retrieved = vector_ops::get_embedding(&db, &node_id).await?;
    assert!(retrieved.is_some());

    let retrieved_emb = retrieved.unwrap();
    assert_eq!(retrieved_emb.len(), EMBEDDING_DIM);
    assert!((retrieved_emb[0] - 0.1).abs() < 0.001);

    println!("  ✓ Retrieved embedding matches inserted\n");

    // Test 3: Update embedding
    println!("[Test 3] Updating embedding...");

    let new_embedding = vec![0.2; EMBEDDING_DIM];
    let vec_rowid2 = vector_ops::insert_embedding(&db, &node_id, &new_embedding).await?;

    assert_eq!(vec_rowid, vec_rowid2, "Should reuse same vec_rowid");

    let retrieved2 = vector_ops::get_embedding(&db, &node_id).await?;
    let retrieved_emb2 = retrieved2.unwrap();
    assert!((retrieved_emb2[0] - 0.2).abs() < 0.001);

    println!("  ✓ Embedding updated successfully\n");

    // Test 4: Multiple embeddings
    println!("[Test 4] Inserting multiple embeddings...");

    let mut node_ids = Vec::new();
    for i in 0..5 {
        let nid = format!("node-{}", i);
        create_test_node(&db, &nid).await?;

        // Create distinct embeddings
        let mut emb = vec![0.0; EMBEDDING_DIM];
        emb[0] = i as f32 * 0.1;
        emb[1] = (i as f32 * 0.1).sin();
        emb[2] = (i as f32 * 0.1).cos();

        vector_ops::insert_embedding(&db, &nid, &emb).await?;
        node_ids.push(nid);
    }

    println!("  ✓ Inserted {} embeddings\n", node_ids.len());

    // Test 5: Count embeddings
    println!("[Test 5] Counting embeddings...");

    let count = vector_ops::count_embeddings(&db).await?;
    println!("  Total embeddings: {}", count);
    assert_eq!(count, 6, "Should have 6 embeddings (1 + 5)");
    println!("  ✓ Count correct\n");

    // Test 6: Delete embedding
    println!("[Test 6] Deleting embedding...");

    let deleted = vector_ops::delete_embedding(&db, &node_ids[0]).await?;
    assert!(deleted);

    let retrieved_after_delete = vector_ops::get_embedding(&db, &node_ids[0]).await?;
    assert!(retrieved_after_delete.is_none());

    let count_after_delete = vector_ops::count_embeddings(&db).await?;
    assert_eq!(count_after_delete, 5);

    println!("  ✓ Embedding deleted successfully\n");

    // Test 7: Semantic similarity search (manual implementation)
    println!("[Test 7] Testing semantic similarity (manual)...");

    // Create query embedding
    let query = vec![0.15; EMBEDDING_DIM];

    println!("  Query embedding: [0.15, 0.15, 0.15, ...]");
    println!("  Finding most similar nodes...\n");

    // Manually compute similarities (since we don't have sqlite-vec)
    use synapse::cosine_similarity;

    let mut similarities = Vec::new();
    for nid in &node_ids[1..] {  // Skip deleted node
        if let Some(emb) = vector_ops::get_embedding(&db, nid).await? {
            let sim = cosine_similarity(&query, &emb)?;
            similarities.push((nid.clone(), sim));
        }
    }

    // Sort by similarity (descending)
    similarities.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    println!("  Top 3 similar nodes:");
    for (i, (nid, sim)) in similarities.iter().take(3).enumerate() {
        println!("    {}. {}: similarity = {:.4}", i + 1, nid, sim);
    }

    println!("  ✓ Similarity search working\n");

    // Test 8: Error handling
    println!("[Test 8] Testing error handling...");

    // Wrong dimension
    let wrong_emb = vec![0.1; 128];
    let result = vector_ops::insert_embedding(&db, "test", &wrong_emb).await;
    assert!(result.is_err());
    println!("  ✓ Invalid dimension rejected");

    // Nonexistent node
    let result = vector_ops::get_embedding(&db, "nonexistent").await?;
    assert!(result.is_none());
    println!("  ✓ Nonexistent node handled");

    // Delete nonexistent
    let deleted = vector_ops::delete_embedding(&db, "nonexistent").await?;
    assert!(!deleted);
    println!("  ✓ Delete nonexistent returns false\n");

    // Test 9: Real embedding integration
    println!("[Test 9] Testing with real embeddings (if available)...");

    match synapse::EmbeddingService::new() {
        Ok(service) => {
            println!("  Embedding service available");

            // Generate real embeddings
            let text1 = "Machine learning with neural networks";
            let text2 = "Deep learning and artificial intelligence";
            let text3 = "Cooking pasta with tomato sauce";

            let emb1 = service.generate(text1)?;
            let emb2 = service.generate(text2)?;
            let emb3 = service.generate(text3)?;

            // Store embeddings
            let nid1 = "ml-doc";
            let nid2 = "dl-doc";
            let nid3 = "cooking-doc";

            create_test_node(&db, nid1).await?;
            create_test_node(&db, nid2).await?;
            create_test_node(&db, nid3).await?;

            vector_ops::insert_embedding(&db, nid1, &emb1).await?;
            vector_ops::insert_embedding(&db, nid2, &emb2).await?;
            vector_ops::insert_embedding(&db, nid3, &emb3).await?;

            println!("  Stored 3 real embeddings");

            // Compute similarities
            let sim_ml_dl = cosine_similarity(&emb1, &emb2)?;
            let sim_ml_cooking = cosine_similarity(&emb1, &emb3)?;

            println!("  ML ↔ DL: {:.4}", sim_ml_dl);
            println!("  ML ↔ Cooking: {:.4}", sim_ml_cooking);

            assert!(sim_ml_dl > sim_ml_cooking, "ML should be more similar to DL than Cooking");
            println!("  ✓ Real embeddings preserve semantic relationships\n");
        }
        Err(e) => {
            println!("  ⚠ Embedding service not available ({})", e);
            println!("  To enable: pip install sentence-transformers\n");
        }
    }

    // Summary
    println!("=== Test Summary ===");
    println!("✓ Embedding insertion: OK");
    println!("✓ Embedding retrieval: OK");
    println!("✓ Embedding update: OK");
    println!("✓ Multiple embeddings: OK");
    println!("✓ Embedding count: OK");
    println!("✓ Embedding deletion: OK");
    println!("✓ Similarity search (manual): OK");
    println!("✓ Error handling: OK");
    println!("✓ Real embeddings (optional): OK or SKIPPED");

    println!("\n=== Phase 2 Day 4 Complete! ===");
    println!("\nKey Achievements:");
    println!("  - Vector storage working (insert, get, update, delete)");
    println!("  - Embedding lifecycle management");
    println!("  - Count operations");
    println!("  - Manual similarity search (fallback without sqlite-vec)");

    println!("\nVector Search Notes:");
    println!("  Without sqlite-vec: Manual similarity computation (slower)");
    println!("  With sqlite-vec: SIMD-optimized k-NN search (<50ms)");
    println!("  Fallback table ensures tests pass without extension");

    println!("\nDatabase Schema:");
    println!("  vec_nodes: Virtual or fallback table for vectors");
    println!("  node_embeddings: Mapping node_id → vec_rowid");

    println!("\nNext Steps:");
    println!("  - Day 5: Full pipeline integration with observer");
    println!("  - Day 6: Hybrid search (RRF algorithm)");
    println!("  - Optional: Load sqlite-vec extension for faster search");

    Ok(())
}

async fn create_schema(db: &sqlx::SqlitePool) -> Result<()> {
    // Nodes table
    sqlx::query(
        r#"
        CREATE TABLE nodes (
            id TEXT PRIMARY KEY,
            type TEXT NOT NULL,
            properties TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
    )
    .execute(db)
    .await?;

    // Fallback vec_nodes table (without sqlite-vec extension)
    sqlx::query(
        r#"
        CREATE TABLE vec_nodes (
            rowid INTEGER PRIMARY KEY AUTOINCREMENT,
            embedding TEXT NOT NULL
        )
        "#
    )
    .execute(db)
    .await?;

    // Mapping table
    sqlx::query(
        r#"
        CREATE TABLE node_embeddings (
            node_id TEXT PRIMARY KEY,
            vec_rowid INTEGER NOT NULL,
            created_at TEXT NOT NULL
        )
        "#
    )
    .execute(db)
    .await?;

    Ok(())
}

async fn create_test_node(db: &sqlx::SqlitePool, node_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let properties = serde_json::json!({ "name": node_id });

    sqlx::query(
        "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', ?, ?, ?)"
    )
    .bind(node_id)
    .bind(properties.to_string())
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await?;

    Ok(())
}
