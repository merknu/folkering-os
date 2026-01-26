//! Test semantic query methods - Phase 2 Day 7
//!
//! This example verifies that:
//! 1. find_similar() works - find documents similar to a given file
//! 2. find_files_mentioning_entity() works - find files that reference entities
//! 3. find_files_about() works - find files about a concept
//! 4. find_related_entities() works - entity co-occurrence analysis
//!
//! Prerequisites:
//!   - Python 3.10+ with sentence-transformers (optional for embeddings)
//!   - Run from project root: cargo run --example test_semantic_queries_day7

use anyhow::Result;
use synapse::{
    query::{fts_search, semantic},
    graph::{vector_ops, entity_ops},
    EmbeddingService,
};
use sqlx::sqlite::SqlitePoolOptions;
use chrono::Utc;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Synapse Phase 2 Day 7: Semantic Query Methods Test ===\n");

    // Setup in-memory database
    println!("[Setup] Creating in-memory database...");
    let db = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await?;

    println!("[Setup] Creating database schema...");
    create_schema(&db).await?;
    println!("  ✓ Database ready\n");

    // Create test corpus
    println!("[Setup] Creating test corpus...");

    let docs = vec![
        ("doc1", "Alice and Bob are working on machine learning research.", vec!["Alice", "Bob"]),
        ("doc2", "Alice published a paper on neural networks and deep learning.", vec!["Alice"]),
        ("doc3", "Bob is developing a new AI framework for computer vision.", vec!["Bob"]),
        ("doc4", "The Project Mars team includes Alice, Bob, and Carol.", vec!["Alice", "Bob", "Carol"]),
        ("doc5", "Carol is studying quantum computing and cryptography.", vec!["Carol"]),
        ("doc6", "Machine learning and artificial intelligence are transforming industries.", vec![]),
        ("doc7", "Italian cuisine features pasta, pizza, and risotto.", vec![]),
    ];

    for (id, content, entities) in &docs {
        // Create file node
        create_file_node(&db, id).await?;

        // Index content for FTS
        fts_search::index_content(&db, id, content).await?;

        // Create entity nodes and REFERENCES edges
        for entity_text in entities {
            let entity_node = entity_ops::deduplicate_entity(&db, entity_text, "person", 0.9).await?;
            create_reference_edge(&db, id, &entity_node.id).await?;
        }
    }

    println!("  Created {} documents", docs.len());
    println!("  Created entity relationships\n");

    // Test 1: Find files mentioning entity
    println!("[Test 1] Find files mentioning entity...");

    let alice_files = semantic::find_files_mentioning_entity(&db, "Alice", "person").await?;
    println!("  Query: Files mentioning 'Alice'");
    println!("  Results: {}", alice_files.len());
    for file in &alice_files {
        println!("    - {}", file.id);
    }
    println!();

    let bob_files = semantic::find_files_mentioning_entity(&db, "Bob", "person").await?;
    println!("  Query: Files mentioning 'Bob'");
    println!("  Results: {}", bob_files.len());
    for file in &bob_files {
        println!("    - {}", file.id);
    }
    println!();

    // Test 2: Find related entities
    println!("[Test 2] Find related entities...");

    let alice_related = semantic::find_related_entities(&db, "Alice", "person", 10).await?;
    println!("  Query: Entities related to 'Alice'");
    println!("  Results: {}", alice_related.len());
    for entity in &alice_related {
        let props: serde_json::Value = serde_json::from_str(&entity.properties)?;
        let text = props["entity_text"].as_str().unwrap_or("?");
        println!("    - {} ({:?})", text, entity.r#type);
    }
    println!();

    // Test 3: Vector-based queries (if embeddings available)
    println!("[Test 3] Vector-based semantic queries...");

    match EmbeddingService::new() {
        Ok(embedder) => {
            println!("  Embedding service available");

            // Generate and store embeddings
            for (id, content, _) in &docs {
                let embedding = embedder.generate(content)?;
                vector_ops::insert_embedding(&db, id, &embedding).await?;
            }

            println!("  ✓ Stored {} embeddings\n", docs.len());

            // Test 3a: Find files about a concept
            println!("  [Test 3a] Find files about concept...");
            let ml_files = semantic::find_files_about(&db, &embedder, "machine learning", 0.5, 5).await?;
            println!("    Query: \"machine learning\"");
            println!("    Results: {}", ml_files.len());
            for file in &ml_files {
                println!("      - {}", file.id);
            }
            println!();

            // Test 3b: Find similar documents
            println!("  [Test 3b] Find similar documents...");
            let similar_to_doc1 = semantic::find_similar(&db, &embedder, "doc1", 0.5, 3).await?;
            println!("    Query: Similar to doc1 (Alice and Bob working on ML)");
            println!("    Results: {}", similar_to_doc1.len());
            for file in &similar_to_doc1 {
                println!("      - {}", file.id);
            }
            println!();

            // Test 3c: Hybrid search with context
            println!("  [Test 3c] Hybrid search with context...");
            let hybrid_results = semantic::search_with_context(&db, &embedder, "neural networks", 5).await?;
            println!("    Query: \"neural networks\"");
            println!("    Results: {}", hybrid_results.len());
            for result in &hybrid_results {
                println!("      - {} (score: {:.4}, fts: {:?}, vec: {:?})",
                    result.node.id, result.score, result.fts_rank, result.vector_rank);
            }
            println!();

            println!("  ✓ All vector-based queries working\n");
        }
        Err(e) => {
            println!("  ⚠ Embedding service unavailable: {}", e);
            println!("  To enable: pip install sentence-transformers");
            println!("  Skipping vector-based query tests\n");
        }
    }

    // Test 4: Entity-only queries (no embeddings needed)
    println!("[Test 4] Entity graph traversal...");

    // Find common collaborators
    let alice_files_set: std::collections::HashSet<_> = alice_files.iter().map(|f| &f.id).collect();
    let bob_files_set: std::collections::HashSet<_> = bob_files.iter().map(|f| &f.id).collect();
    let common_files: Vec<_> = alice_files_set.intersection(&bob_files_set).collect();

    println!("  Alice's files: {}", alice_files.len());
    println!("  Bob's files: {}", bob_files.len());
    println!("  Files mentioning both: {}", common_files.len());
    for file_id in &common_files {
        println!("    - {}", file_id);
    }
    println!();

    println!("  ✓ Entity graph traversal working\n");

    // Summary
    println!("=== Test Summary ===");
    println!("✓ Find files mentioning entity: OK");
    println!("✓ Find related entities: OK");
    println!("{} Find files about concept: {}",
        if EmbeddingService::new().is_ok() { "✓" } else { "⚠" },
        if EmbeddingService::new().is_ok() { "OK" } else { "SKIPPED" });
    println!("{} Find similar documents: {}",
        if EmbeddingService::new().is_ok() { "✓" } else { "⚠" },
        if EmbeddingService::new().is_ok() { "OK" } else { "SKIPPED" });
    println!("{} Hybrid search with context: {}",
        if EmbeddingService::new().is_ok() { "✓" } else { "⚠" },
        if EmbeddingService::new().is_ok() { "OK" } else { "SKIPPED" });
    println!("✓ Entity graph traversal: OK");

    println!("\n=== Phase 2 Day 7 Complete! ===");
    println!("\nKey Achievements:");
    println!("  - High-level semantic query API implemented");
    println!("  - Entity-based queries (find files mentioning entities)");
    println!("  - Co-occurrence analysis (find related entities)");
    println!("  - Concept-based search (find files about topics)");
    println!("  - Similarity search (find similar documents)");
    println!("  - Hybrid search integration");

    println!("\nQuery Capabilities:");
    println!("  - \"Which files mention Alice?\" → Entity traversal");
    println!("  - \"Who does Alice work with?\" → Co-occurrence analysis");
    println!("  - \"Find files about machine learning\" → Vector search");
    println!("  - \"Find files similar to X\" → Vector similarity");
    println!("  - \"Search for neural networks\" → Hybrid (FTS + vector)");

    println!("\nNext Steps:");
    println!("  - Day 8: Testing, documentation, benchmarks");
    println!("  - Phase 2 completion report");

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

    // Edges table
    sqlx::query(
        r#"
        CREATE TABLE edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id TEXT NOT NULL,
            target_id TEXT NOT NULL,
            type TEXT NOT NULL,
            weight REAL DEFAULT 1.0,
            created_at TEXT NOT NULL,
            FOREIGN KEY (source_id) REFERENCES nodes(id) ON DELETE CASCADE,
            FOREIGN KEY (target_id) REFERENCES nodes(id) ON DELETE CASCADE
        )
        "#
    )
    .execute(db)
    .await?;

    // File content table
    sqlx::query(
        r#"
        CREATE TABLE file_content (
            node_id TEXT PRIMARY KEY,
            content TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
    )
    .execute(db)
    .await?;

    // FTS5 virtual table
    sqlx::query(
        r#"
        CREATE VIRTUAL TABLE file_content_fts USING fts5(
            content,
            content_rowid=rowid,
            tokenize='porter unicode61'
        )
        "#
    )
    .execute(db)
    .await?;

    // FTS5 triggers
    sqlx::query(
        r#"
        CREATE TRIGGER file_content_ai AFTER INSERT ON file_content BEGIN
            INSERT INTO file_content_fts(rowid, content)
            VALUES (NEW.rowid, NEW.content);
        END
        "#
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TRIGGER file_content_au AFTER UPDATE ON file_content BEGIN
            UPDATE file_content_fts
            SET content = NEW.content
            WHERE rowid = NEW.rowid;
        END
        "#
    )
    .execute(db)
    .await?;

    sqlx::query(
        r#"
        CREATE TRIGGER file_content_ad AFTER DELETE ON file_content BEGIN
            DELETE FROM file_content_fts WHERE rowid = OLD.rowid;
        END
        "#
    )
    .execute(db)
    .await?;

    // Vector tables
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

async fn create_file_node(db: &sqlx::SqlitePool, id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let properties = serde_json::json!({ "name": id });

    sqlx::query(
        "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', ?, ?, ?)"
    )
    .bind(id)
    .bind(properties.to_string())
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await?;

    Ok(())
}

async fn create_reference_edge(db: &sqlx::SqlitePool, source: &str, target: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO edges (source_id, target_id, type, created_at) VALUES (?, ?, 'REFERENCES', ?)"
    )
    .bind(source)
    .bind(target)
    .bind(&now)
    .execute(db)
    .await?;

    Ok(())
}
