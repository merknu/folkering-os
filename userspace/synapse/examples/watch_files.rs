//! Demonstrate Observer watching files and creating edges in real-time

use synapse::{GraphDB, Node, NodeType, QueryEngine, Observer};
use sqlx::SqlitePool;
use anyhow::Result;
use serde_json::json;
use std::fs;

#[tokio::main]
async fn main() -> Result<()> {
    println!("🔍 Testing Synapse Observer with Real File Events\n");

    // Create temporary directory for testing
    let test_dir = std::env::temp_dir().join("synapse_test");
    if test_dir.exists() {
        fs::remove_dir_all(&test_dir)?;
    }
    fs::create_dir_all(&test_dir)?;
    println!("📁 Test directory: {:?}\n", test_dir);

    // Create database
    let db_path = test_dir.join("test.db");
    let db = SqlitePool::connect(&format!("sqlite:{}?mode=rwc", db_path.display())).await?;

    // Run migrations
    println!("📦 Running migrations...");
    run_migrations(&db).await?;
    println!("✅ Migrations complete\n");

    let graph = GraphDB::new(db.clone());
    let query = QueryEngine::new(db.clone());

    // Create test files as nodes
    println!("📝 Creating file nodes...\n");

    let file1 = Node::new(
        NodeType::File,
        json!({
            "name": "test1.txt",
            "size": 100
        })
    );
    graph.create_node(&file1).await?;
    let file1_path = test_dir.join("test1.txt");
    graph.register_path(&file1.id, &file1_path.to_string_lossy()).await?;
    println!("  Created: test1.txt ({})", file1.id);

    let file2 = Node::new(
        NodeType::File,
        json!({
            "name": "test2.txt",
            "size": 200
        })
    );
    graph.create_node(&file2).await?;
    let file2_path = test_dir.join("test2.txt");
    graph.register_path(&file2.id, &file2_path.to_string_lossy()).await?;
    println!("  Created: test2.txt ({})", file2.id);

    let file3 = Node::new(
        NodeType::File,
        json!({
            "name": "test3.txt",
            "size": 300
        })
    );
    graph.create_node(&file3).await?;
    let file3_path = test_dir.join("test3.txt");
    graph.register_path(&file3.id, &file3_path.to_string_lossy()).await?;
    println!("  Created: test3.txt ({})\n", file3.id);

    // Create user
    let user = Node::new(
        NodeType::Person,
        json!({
            "name": "TestUser",
            "email": "test@example.com"
        })
    );
    graph.create_node(&user).await?;
    println!("  Created: TestUser ({})\n", user.id);

    // Create observer WITH database connection
    let observer = Observer::with_db(db.clone());

    // Simulate file access patterns
    println!("🎬 Simulating file access patterns...\n");

    // Session 1: Access file1 and file2 together
    println!("Session 1: Opening file1 and file2 (should create CO_OCCURRED edge)");
    observer.handle_file_access_with_id(file1.id.clone()).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    observer.handle_file_access_with_id(file2.id.clone()).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Check for co-occurrence edge
    let cooccur = query.find_co_occurring(&file1.id, 0.0).await?;
    println!("  ✓ Files co-occurring with test1.txt: {}", cooccur.len());
    assert!(cooccur.len() > 0, "Should have created CO_OCCURRED edge");

    // Session 2: Access file1 and file2 again (should strengthen edge)
    println!("\nSession 2: Opening file1 and file2 again (should strengthen edge)");
    // Wait 6 minutes to start new session (simulated by clearing session)
    observer.clear_session().await;
    observer.handle_file_access_with_id(file1.id.clone()).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    observer.handle_file_access_with_id(file2.id.clone()).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Check edge weight increased
    let edges = graph.get_node_edges(&file1.id).await?;
    let cooccur_edge = edges.iter()
        .find(|e| e.r#type.as_str() == "CO_OCCURRED")
        .expect("Should have CO_OCCURRED edge");
    println!("  ✓ CO_OCCURRED weight: {:.2} (should be > 0.3)", cooccur_edge.weight);
    assert!(cooccur_edge.weight > 0.3, "Weight should increase with more sessions");

    // Simulate edit events
    println!("\nEditing files (should create EDITED_BY edges)");
    observer.handle_file_edit_with_user(file1.id.clone(), user.id.clone()).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    observer.handle_file_edit_with_user(file1.id.clone(), user.id.clone()).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    observer.handle_file_edit_with_user(file2.id.clone(), user.id.clone()).await;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Check edit edges
    let edited = query.find_edited_by("TestUser").await?;
    println!("  ✓ Files edited by TestUser: {}", edited.len());
    assert_eq!(edited.len(), 2, "Should have edited 2 files");

    // Get final statistics
    println!("\n📊 Final Graph Statistics:");
    let stats = graph.get_stats().await?;
    println!("  Nodes: {}", stats.node_count);
    println!("  Edges: {}", stats.edge_count);
    println!("  Avg weight: {:.2}", stats.avg_edge_weight);

    // Show all edges
    println!("\n🔗 All Edges Created:");
    for i in 1..=3 {
        let file_id = match i {
            1 => &file1.id,
            2 => &file2.id,
            _ => &file3.id,
        };
        let edges = graph.get_node_edges(file_id).await?;
        for edge in edges {
            let target = graph.get_node(&edge.target_id).await?.unwrap();
            let target_name = target.get_property("name").unwrap_or_default();
            println!("  test{}.txt -> {} ({}) [weight: {:.2}]",
                i,
                target_name,
                edge.r#type.as_str(),
                edge.weight
            );
        }
    }

    // Cleanup
    println!("\n🧹 Cleaning up test directory...");
    // Close database before cleanup
    db.close().await;
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    fs::remove_dir_all(&test_dir)?;

    println!("\n✅ All observer tests passed! Edges are created in real-time.\n");

    Ok(())
}

async fn run_migrations(db: &SqlitePool) -> Result<()> {
    // Same migrations as populate_graph example
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

    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS edges (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            source_id TEXT NOT NULL,
            target_id TEXT NOT NULL,
            type TEXT NOT NULL,
            weight REAL NOT NULL DEFAULT 0.5,
            properties TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (source_id) REFERENCES nodes(id) ON DELETE CASCADE,
            FOREIGN KEY (target_id) REFERENCES nodes(id) ON DELETE CASCADE,
            UNIQUE(source_id, target_id, type),
            CHECK (type IN (
                'CONTAINS', 'EDITED_BY', 'OPENED_WITH', 'MENTIONS',
                'SHARED_WITH', 'HAPPENED_DURING', 'CO_OCCURRED',
                'SIMILAR_TO', 'DEPENDS_ON', 'REFERENCES', 'PARENT_OF', 'TAGGED_WITH'
            ))
        )
    "#).execute(db).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id)").execute(db).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_edges_weight ON edges(weight DESC)").execute(db).await?;

    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS project_meta (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
    "#).execute(db).await?;

    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS file_paths (
            node_id TEXT PRIMARY KEY NOT NULL,
            path TEXT NOT NULL UNIQUE,
            FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
        )
    "#).execute(db).await?;

    Ok(())
}
