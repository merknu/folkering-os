//! Test entity storage and linking - Phase 2 Day 2
//!
//! This example verifies that:
//! 1. Entity nodes can be created in the database
//! 2. Entities are linked to files via REFERENCES edges
//! 3. Entity deduplication works
//! 4. Queries like "Which files mention Alice?" return correct results
//!
//! Prerequisites:
//!   - Python 3.10+ with GLiNER installed
//!   - Run from project root: cargo run --example test_entity_storage_day2

use anyhow::Result;
use synapse::{EntityPipeline, GLiNERService};
use synapse::graph::entity_ops;
use synapse::models::{Node, NodeType};
use sqlx::sqlite::SqlitePoolOptions;
use std::io::Write;
use tempfile::TempDir;
use uuid::Uuid;
use chrono::Utc;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Synapse Phase 2 Day 2: Entity Storage Test ===\n");

    // Setup in-memory database
    println!("[Setup] Creating in-memory database...");
    let db = SqlitePoolOptions::new()
        .connect("sqlite::memory:")
        .await?;

    // Create schema
    println!("[Setup] Creating database schema...");
    create_schema(&db).await?;
    println!("  ✓ Database ready\n");

    // Test 1: Create entity nodes directly
    println!("[Test 1] Creating entity nodes...");

    let alice = entity_ops::create_entity_node(
        &db,
        "Alice",
        "person",
        0.95,
    ).await?;

    println!("  ✓ Created entity: {} ({})", "Alice", alice.id);
    assert_eq!(alice.r#type, NodeType::Person);
    assert!(alice.properties.contains("Alice"));

    let bob = entity_ops::create_entity_node(
        &db,
        "Bob",
        "person",
        0.92,
    ).await?;

    println!("  ✓ Created entity: {} ({})", "Bob", bob.id);

    let project_mars = entity_ops::create_entity_node(
        &db,
        "Project Mars",
        "project",
        0.88,
    ).await?;

    println!("  ✓ Created entity: {} ({})\n", "Project Mars", project_mars.id);

    // Test 2: Find entities by text
    println!("[Test 2] Finding entities by text...");

    let found_alice = entity_ops::find_entity_by_text(&db, "Alice").await?;
    assert!(found_alice.is_some());
    println!("  ✓ Found Alice: {}", found_alice.unwrap().id);

    let not_found = entity_ops::find_entity_by_text(&db, "Charlie").await?;
    assert!(not_found.is_none());
    println!("  ✓ Charlie not found (as expected)\n");

    // Test 3: Entity deduplication
    println!("[Test 3] Testing entity deduplication...");

    // Try to create Alice again
    let alice_dup = entity_ops::deduplicate_entity(
        &db,
        "Alice",
        "person",
        0.90,
    ).await?;

    assert_eq!(alice.id, alice_dup.id);
    println!("  ✓ Deduplication works: same ID returned");

    // Count entities (should still be 3)
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM nodes WHERE type IN ('person', 'project')"
    )
    .fetch_one(&db)
    .await?;

    assert_eq!(count.0, 3);
    println!("  ✓ Total entities: {} (no duplicates created)\n", count.0);

    // Test 4: Link files to entities
    println!("[Test 4] Linking files to entities...");

    // Create file node
    let file1_id = Uuid::new_v4().to_string();
    create_file_node(&db, &file1_id, "team.md").await?;

    // Link file to entities
    let edge1 = entity_ops::link_resource_to_entity(
        &db,
        &file1_id,
        &alice.id,
        0.95,
    ).await?;

    println!("  ✓ Linked team.md → Alice");

    let edge2 = entity_ops::link_resource_to_entity(
        &db,
        &file1_id,
        &bob.id,
        0.92,
    ).await?;

    println!("  ✓ Linked team.md → Bob");

    let edge3 = entity_ops::link_resource_to_entity(
        &db,
        &file1_id,
        &project_mars.id,
        0.88,
    ).await?;

    println!("  ✓ Linked team.md → Project Mars\n");

    // Test 5: Get entities for file
    println!("[Test 5] Querying entities for file...");

    let entities = entity_ops::get_entities_for_file(&db, &file1_id).await?;

    println!("  File 'team.md' references {} entities:", entities.len());
    for entity in &entities {
        let props: serde_json::Value = serde_json::from_str(&entity.properties)?;
        let name = props.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!("    - {} ({})", name, entity.r#type.as_str());
    }

    assert_eq!(entities.len(), 3);
    println!("  ✓ All entities retrieved\n");

    // Test 6: Get files for entity
    println!("[Test 6] Querying files that mention entity...");

    // Create another file
    let file2_id = Uuid::new_v4().to_string();
    create_file_node(&db, &file2_id, "project_notes.md").await?;

    // Link it to Alice and Project Mars
    entity_ops::link_resource_to_entity(&db, &file2_id, &alice.id, 0.90).await?;
    entity_ops::link_resource_to_entity(&db, &file2_id, &project_mars.id, 0.85).await?;

    // Query: Which files mention Alice?
    let files_with_alice = entity_ops::get_files_for_entity(&db, &alice.id).await?;

    println!("  Files mentioning 'Alice': {}", files_with_alice.len());
    for file in &files_with_alice {
        let props: serde_json::Value = serde_json::from_str(&file.properties)?;
        let name = props.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!("    - {}", name);
    }

    assert_eq!(files_with_alice.len(), 2);
    println!("  ✓ Found both files mentioning Alice\n");

    // Test 7: End-to-end pipeline (with GLiNER)
    println!("[Test 7] Testing full entity extraction pipeline...");

    match GLiNERService::new() {
        Ok(gliner) => {
            // Create temporary file with text
            let temp_dir = TempDir::new()?;
            let test_file = temp_dir.path().join("test_doc.txt");
            let mut file = std::fs::File::create(&test_file)?;
            file.write_all(b"Alice and Bob are collaborating with NASA on the Mars mission.")?;

            // Create file node
            let file3_id = Uuid::new_v4().to_string();
            create_file_node(&db, &file3_id, "test_doc.txt").await?;

            // Extract and process entities
            println!("  Processing file: test_doc.txt");
            println!("  Content: \"Alice and Bob are collaborating with NASA on the Mars mission.\"");

            let entities = synapse::process_file_for_entities(
                &db,
                &gliner,
                &test_file,
                &file3_id,
                &["person", "organization"],
                0.5,
            ).await?;

            println!("  ✓ Extracted {} entities:", entities.len());
            for entity in &entities {
                let props: serde_json::Value = serde_json::from_str(&entity.properties)?;
                let name = props.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                let confidence = props.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0);
                println!("    - {} ({:.2})", name, confidence);
            }

            // Should find Alice, Bob, NASA (with deduplication for Alice/Bob)
            assert!(entities.len() >= 2, "Expected at least 2 entities");
            println!("  ✓ Full pipeline working\n");
        }
        Err(e) => {
            println!("  ⚠ Skipping pipeline test (GLiNER not available)");
            println!("  Error: {}", e);
            println!("  To enable: Install Python 3.10+ and run: pip install gliner\n");
        }
    }

    // Summary
    println!("=== Test Summary ===");
    println!("✓ Entity node creation: OK");
    println!("✓ Entity search by text: OK");
    println!("✓ Entity deduplication: OK");
    println!("✓ File→Entity linking: OK");
    println!("✓ Get entities for file: OK");
    println!("✓ Get files for entity: OK");
    println!("✓ Full extraction pipeline: OK (if GLiNER available)");

    println!("\n=== Phase 2 Day 2 Complete! ===");
    println!("\nKey Achievements:");
    println!("  - Entity CRUD operations working");
    println!("  - Entities stored as nodes in graph database");
    println!("  - REFERENCES edges link files to entities");
    println!("  - Entity deduplication prevents duplicates");
    println!("  - Query API: 'Which files mention X?' functional");

    println!("\nNext Steps:");
    println!("  - Day 3: Embedding generation (sentence-transformers)");
    println!("  - Day 4: Vector search (sqlite-vec)");
    println!("  - Day 5: Full pipeline integration with observer");

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
            updated_at TEXT NOT NULL,
            CHECK (type IN ('file', 'person', 'app', 'event', 'tag', 'project', 'location'))
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
            properties TEXT,
            created_at TEXT NOT NULL,
            CHECK (weight >= 0.0 AND weight <= 1.0)
        )
        "#
    )
    .execute(db)
    .await?;

    Ok(())
}

async fn create_file_node(
    db: &sqlx::SqlitePool,
    file_id: &str,
    name: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let properties = serde_json::json!({ "name": name });

    sqlx::query(
        r#"
        INSERT INTO nodes (id, type, properties, created_at, updated_at)
        VALUES (?, 'file', ?, ?, ?)
        "#
    )
    .bind(file_id)
    .bind(properties.to_string())
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await?;

    Ok(())
}
