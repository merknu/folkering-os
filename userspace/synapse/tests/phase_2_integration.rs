//! Phase 2 Integration Tests
//!
//! Comprehensive tests covering all Phase 2 functionality:
//! - Entity extraction and storage
//! - Embedding generation
//! - Vector search
//! - Hybrid search (FTS + vector)
//! - Semantic query methods
//! - Full pipeline integration

use anyhow::Result;
use synapse::{
    query::{fts_search, hybrid_search, semantic},
    graph::{entity_ops, vector_ops},
    EmbeddingService,
};
use sqlx::sqlite::SqlitePoolOptions;
use chrono::Utc;

async fn setup_test_db() -> sqlx::SqlitePool {
    let db = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await
        .unwrap();

    // Create full schema
    create_full_schema(&db).await.unwrap();

    db
}

async fn create_full_schema(db: &sqlx::SqlitePool) -> Result<()> {
    // Nodes
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
    ).execute(db).await?;

    // Edges
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
    ).execute(db).await?;

    // File content
    sqlx::query(
        r#"
        CREATE TABLE file_content (
            node_id TEXT PRIMARY KEY,
            content TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#
    ).execute(db).await?;

    // FTS5
    sqlx::query(
        r#"
        CREATE VIRTUAL TABLE file_content_fts USING fts5(
            content,
            content_rowid=rowid,
            tokenize='porter unicode61'
        )
        "#
    ).execute(db).await?;

    // FTS triggers
    sqlx::query(
        r#"
        CREATE TRIGGER file_content_ai AFTER INSERT ON file_content BEGIN
            INSERT INTO file_content_fts(rowid, content)
            VALUES (NEW.rowid, NEW.content);
        END
        "#
    ).execute(db).await?;

    sqlx::query(
        r#"
        CREATE TRIGGER file_content_au AFTER UPDATE ON file_content BEGIN
            UPDATE file_content_fts
            SET content = NEW.content
            WHERE rowid = NEW.rowid;
        END
        "#
    ).execute(db).await?;

    sqlx::query(
        r#"
        CREATE TRIGGER file_content_ad AFTER DELETE ON file_content BEGIN
            DELETE FROM file_content_fts WHERE rowid = OLD.rowid;
        END
        "#
    ).execute(db).await?;

    // Vector tables
    sqlx::query(
        r#"
        CREATE TABLE vec_nodes (
            rowid INTEGER PRIMARY KEY AUTOINCREMENT,
            embedding TEXT NOT NULL
        )
        "#
    ).execute(db).await?;

    sqlx::query(
        r#"
        CREATE TABLE node_embeddings (
            node_id TEXT PRIMARY KEY,
            vec_rowid INTEGER NOT NULL,
            created_at TEXT NOT NULL
        )
        "#
    ).execute(db).await?;

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

// ============================================================================
// Test Suite
// ============================================================================

#[tokio::test]
async fn test_entity_extraction_and_storage() {
    let db = setup_test_db().await;

    // Create entity
    let entity = entity_ops::create_entity_node(&db, "Alice", "person", 0.95)
        .await
        .unwrap();

    assert_eq!(entity.r#type.as_str(), "person");

    // Find entity
    let found = entity_ops::find_entity_by_text(&db, "Alice")
        .await
        .unwrap();

    assert!(found.is_some());
    assert_eq!(found.unwrap().id, entity.id);
}

#[tokio::test]
async fn test_entity_deduplication() {
    let db = setup_test_db().await;

    // Create entity twice with deduplication
    let entity1 = entity_ops::deduplicate_entity(&db, "Alice", "person", 0.95)
        .await
        .unwrap();

    let entity2 = entity_ops::deduplicate_entity(&db, "Alice", "person", 0.90)
        .await
        .unwrap();

    // Should be same entity
    assert_eq!(entity1.id, entity2.id);
}

#[tokio::test]
async fn test_entity_file_linking() {
    let db = setup_test_db().await;

    // Create file and entity
    create_file_node(&db, "doc1").await.unwrap();
    let entity = entity_ops::create_entity_node(&db, "Alice", "person", 0.95)
        .await
        .unwrap();

    // Link them
    entity_ops::link_resource_to_entity(&db, "doc1", &entity.id, 0.95)
        .await
        .unwrap();

    // Verify edge exists
    let files = entity_ops::get_files_for_entity(&db, &entity.id)
        .await
        .unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, "doc1");
}

#[tokio::test]
async fn test_fts_search() {
    let db = setup_test_db().await;

    // Index content
    create_file_node(&db, "doc1").await.unwrap();
    create_file_node(&db, "doc2").await.unwrap();

    fts_search::index_content(&db, "doc1", "Machine learning with neural networks")
        .await
        .unwrap();
    fts_search::index_content(&db, "doc2", "Cooking pasta with tomato sauce")
        .await
        .unwrap();

    // Search
    let results = fts_search::search(&db, "machine learning", 10)
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].node.id, "doc1");
}

#[tokio::test]
async fn test_fts_content_retrieval() {
    let db = setup_test_db().await;

    create_file_node(&db, "doc1").await.unwrap();
    let content = "Test content";

    fts_search::index_content(&db, "doc1", content)
        .await
        .unwrap();

    let retrieved = fts_search::get_content(&db, "doc1")
        .await
        .unwrap();

    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap(), content);
}

#[tokio::test]
async fn test_fts_count_indexed() {
    let db = setup_test_db().await;

    let count1 = fts_search::count_indexed(&db).await.unwrap();
    assert_eq!(count1, 0);

    create_file_node(&db, "doc1").await.unwrap();
    fts_search::index_content(&db, "doc1", "Content 1").await.unwrap();

    let count2 = fts_search::count_indexed(&db).await.unwrap();
    assert_eq!(count2, 1);
}

#[tokio::test]
async fn test_semantic_find_files_mentioning_entity() {
    let db = setup_test_db().await;

    // Setup
    create_file_node(&db, "doc1").await.unwrap();
    let entity = entity_ops::deduplicate_entity(&db, "Alice", "person", 0.95)
        .await
        .unwrap();
    create_reference_edge(&db, "doc1", &entity.id).await.unwrap();

    // Query
    let files = semantic::find_files_mentioning_entity(&db, "Alice", "person")
        .await
        .unwrap();

    assert_eq!(files.len(), 1);
    assert_eq!(files[0].id, "doc1");
}

#[tokio::test]
async fn test_semantic_find_related_entities() {
    let db = setup_test_db().await;

    // Create file and entities
    create_file_node(&db, "doc1").await.unwrap();

    let alice = entity_ops::deduplicate_entity(&db, "Alice", "person", 0.95).await.unwrap();
    let bob = entity_ops::deduplicate_entity(&db, "Bob", "person", 0.95).await.unwrap();

    create_reference_edge(&db, "doc1", &alice.id).await.unwrap();
    create_reference_edge(&db, "doc1", &bob.id).await.unwrap();

    // Query: who is related to Alice?
    let related = semantic::find_related_entities(&db, "Alice", "person", 10)
        .await
        .unwrap();

    // Should find Bob (not Alice)
    assert_eq!(related.len(), 1);
    assert_eq!(related[0].id, bob.id);
}

#[tokio::test]
async fn test_vector_insertion_and_retrieval() {
    let db = setup_test_db().await;

    create_file_node(&db, "doc1").await.unwrap();

    // Create test embedding
    let embedding: Vec<f32> = (0..384).map(|i| i as f32 / 384.0).collect();

    // Insert
    vector_ops::insert_embedding(&db, "doc1", &embedding)
        .await
        .unwrap();

    // Retrieve
    let retrieved = vector_ops::get_embedding(&db, "doc1")
        .await
        .unwrap();

    assert!(retrieved.is_some());
    let retrieved_emb = retrieved.unwrap();
    assert_eq!(retrieved_emb.len(), 384);
    assert_eq!(retrieved_emb[0], embedding[0]);
}

#[tokio::test]
async fn test_incremental_updates() {
    let db = setup_test_db().await;

    create_file_node(&db, "doc1").await.unwrap();

    // Initial content
    fts_search::index_content(&db, "doc1", "Initial content").await.unwrap();
    let count1 = fts_search::count_indexed(&db).await.unwrap();
    assert_eq!(count1, 1);

    // Update content
    fts_search::index_content(&db, "doc1", "Updated content").await.unwrap();
    let count2 = fts_search::count_indexed(&db).await.unwrap();
    assert_eq!(count2, 1); // Should still be 1, not 2

    // Verify content updated
    let content = fts_search::get_content(&db, "doc1").await.unwrap();
    assert_eq!(content.unwrap(), "Updated content");
}

#[tokio::test]
async fn test_delete_content() {
    let db = setup_test_db().await;

    create_file_node(&db, "doc1").await.unwrap();
    fts_search::index_content(&db, "doc1", "Content").await.unwrap();

    let deleted = fts_search::delete_content(&db, "doc1").await.unwrap();
    assert!(deleted);

    let content = fts_search::get_content(&db, "doc1").await.unwrap();
    assert!(content.is_none());

    let count = fts_search::count_indexed(&db).await.unwrap();
    assert_eq!(count, 0);
}

// ============================================================================
// Tests requiring embedding service (skipped if not available)
// ============================================================================

#[tokio::test]
async fn test_embedding_generation() {
    let embedder = match EmbeddingService::new() {
        Ok(e) => e,
        Err(_) => {
            println!("⚠ Skipping: sentence-transformers not installed");
            return;
        }
    };

    let text = "Machine learning with neural networks";
    let embedding = embedder.generate(text).unwrap();

    assert_eq!(embedding.len(), 384);
    assert!(embedding.iter().all(|&x| x.is_finite()));
}

#[tokio::test]
async fn test_vector_search() {
    let db = setup_test_db().await;

    let embedder = match EmbeddingService::new() {
        Ok(e) => e,
        Err(_) => {
            println!("⚠ Skipping: sentence-transformers not installed");
            return;
        }
    };

    // Create documents with embeddings
    let docs = vec![
        ("doc1", "Machine learning with neural networks"),
        ("doc2", "Cooking pasta with tomato sauce"),
        ("doc3", "Deep learning for computer vision"),
    ];

    for (id, content) in &docs {
        create_file_node(&db, id).await.unwrap();
        let embedding = embedder.generate(content).unwrap();
        vector_ops::insert_embedding(&db, id, &embedding).await.unwrap();
    }

    // Search for ML-related content
    let query_emb = embedder.generate("artificial intelligence").unwrap();
    let results = vector_ops::search_similar(&db, &query_emb, 2).await.unwrap();

    assert_eq!(results.len(), 2);
    // Should find ML and deep learning docs, not pasta
    assert!(results.iter().any(|(n, _)| n.id == "doc1" || n.id == "doc3"));
    assert!(!results.iter().any(|(n, _)| n.id == "doc2"));
}

#[tokio::test]
async fn test_hybrid_search() {
    let db = setup_test_db().await;

    let embedder = match EmbeddingService::new() {
        Ok(e) => e,
        Err(_) => {
            println!("⚠ Skipping: sentence-transformers not installed");
            return;
        }
    };

    // Create documents
    let docs = vec![
        ("doc1", "Machine learning with neural networks and deep learning"),
        ("doc2", "Artificial intelligence and machine learning applications"),
        ("doc3", "Cooking pasta with tomato sauce"),
    ];

    for (id, content) in &docs {
        create_file_node(&db, id).await.unwrap();
        fts_search::index_content(&db, id, content).await.unwrap();
        let embedding = embedder.generate(content).unwrap();
        vector_ops::insert_embedding(&db, id, &embedding).await.unwrap();
    }

    // Hybrid search
    let results = hybrid_search::search(&db, &embedder, "machine learning", 3)
        .await
        .unwrap();

    assert!(!results.is_empty());
    // Should find ML docs
    assert!(results.iter().any(|r| r.node.id == "doc1" || r.node.id == "doc2"));
}

#[tokio::test]
async fn test_semantic_find_similar() {
    let db = setup_test_db().await;

    let embedder = match EmbeddingService::new() {
        Ok(e) => e,
        Err(_) => {
            println!("⚠ Skipping: sentence-transformers not installed");
            return;
        }
    };

    // Create documents
    let docs = vec![
        ("doc1", "Machine learning research paper"),
        ("doc2", "Deep learning tutorial"),
        ("doc3", "Cooking recipes"),
    ];

    for (id, content) in &docs {
        create_file_node(&db, id).await.unwrap();
        let embedding = embedder.generate(content).unwrap();
        vector_ops::insert_embedding(&db, id, &embedding).await.unwrap();
    }

    // Find similar to doc1
    let similar = semantic::find_similar(&db, &embedder, "doc1", 0.3, 2)
        .await
        .unwrap();

    // Should find doc2 (ML-related), not doc3 (cooking)
    assert!(similar.iter().any(|n| n.id == "doc2"));
    assert!(!similar.iter().any(|n| n.id == "doc3"));
    assert!(!similar.iter().any(|n| n.id == "doc1")); // Exclude source
}

#[tokio::test]
async fn test_semantic_find_files_about() {
    let db = setup_test_db().await;

    let embedder = match EmbeddingService::new() {
        Ok(e) => e,
        Err(_) => {
            println!("⚠ Skipping: sentence-transformers not installed");
            return;
        }
    };

    // Create documents
    let docs = vec![
        ("doc1", "Machine learning with neural networks"),
        ("doc2", "Cooking pasta"),
    ];

    for (id, content) in &docs {
        create_file_node(&db, id).await.unwrap();
        let embedding = embedder.generate(content).unwrap();
        vector_ops::insert_embedding(&db, id, &embedding).await.unwrap();
    }

    // Find files about ML
    let files = semantic::find_files_about(&db, &embedder, "artificial intelligence", 0.3, 5)
        .await
        .unwrap();

    assert!(files.iter().any(|n| n.id == "doc1"));
    // Might not find doc2 (cooking) depending on threshold
}

// ============================================================================
// End-to-End Integration Test
// ============================================================================

#[tokio::test]
async fn test_end_to_end_pipeline() {
    let db = setup_test_db().await;

    // This test works without embedding service

    // 1. Create file
    create_file_node(&db, "team_doc").await.unwrap();

    // 2. Index content
    let content = "Alice and Bob are working on the Mars project";
    fts_search::index_content(&db, "team_doc", content).await.unwrap();

    // 3. Extract entities (manual for test)
    let alice = entity_ops::deduplicate_entity(&db, "Alice", "person", 0.95).await.unwrap();
    let bob = entity_ops::deduplicate_entity(&db, "Bob", "person", 0.95).await.unwrap();

    create_reference_edge(&db, "team_doc", &alice.id).await.unwrap();
    create_reference_edge(&db, "team_doc", &bob.id).await.unwrap();

    // 4. FTS search
    let fts_results = fts_search::search(&db, "Alice Bob", 10).await.unwrap();
    assert_eq!(fts_results.len(), 1);
    assert_eq!(fts_results[0].node.id, "team_doc");

    // 5. Entity queries
    let alice_files = semantic::find_files_mentioning_entity(&db, "Alice", "person")
        .await
        .unwrap();
    assert_eq!(alice_files.len(), 1);

    let bob_related = semantic::find_related_entities(&db, "Bob", "person", 10)
        .await
        .unwrap();
    assert!(bob_related.iter().any(|e| e.id == alice.id));

    println!("✅ End-to-end pipeline test passed");
}
