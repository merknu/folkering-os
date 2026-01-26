//! Entity operations for the knowledge graph.
//!
//! Entities represent people, projects, organizations, and concepts mentioned in files.
//! This module provides CRUD operations and linking functionality.

use crate::models::{Node, NodeType, Edge, EdgeType};
use crate::neural::Entity;
use sqlx::SqlitePool;
use anyhow::{Result, Context};
use serde_json::json;
use uuid::Uuid;
use chrono::Utc;

/// Entity node with metadata
#[derive(Debug, Clone)]
pub struct EntityNode {
    pub node: Node,
    pub text: String,
    pub label: String,
    pub confidence: f32,
}

/// Map GLiNER labels to NodeType
fn label_to_node_type(label: &str) -> NodeType {
    match label.to_lowercase().as_str() {
        "person" => NodeType::Person,
        "project" => NodeType::Project,
        "location" => NodeType::Location,
        "organization" | "org" => NodeType::App,  // Use App for organizations
        "concept" | "technology" | "tool" => NodeType::Tag,  // Use Tag for concepts
        _ => NodeType::Tag,  // Default to Tag for unknown types
    }
}

/// Create an entity node
///
/// # Arguments
/// * `db` - Database connection
/// * `text` - Entity text (e.g., "Alice", "Project Mars")
/// * `label` - Entity label (e.g., "person", "project")
/// * `confidence` - Extraction confidence (0.0 - 1.0)
///
/// # Returns
/// Created node
pub async fn create_entity_node(
    db: &SqlitePool,
    text: &str,
    label: &str,
    confidence: f32,
) -> Result<Node> {
    let node_type = label_to_node_type(label);
    let node_id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    // Create properties JSON
    let properties = json!({
        "name": text,
        "label": label,
        "confidence": confidence,
        "entity_text": text,  // For searching
    });

    // Insert node
    sqlx::query(
        r#"
        INSERT INTO nodes (id, type, properties, created_at, updated_at)
        VALUES (?, ?, ?, ?, ?)
        "#
    )
    .bind(&node_id)
    .bind(node_type.as_str())
    .bind(properties.to_string())
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await
    .context("Failed to insert entity node")?;

    // Fetch the created node
    let node: Node = sqlx::query_as(
        "SELECT * FROM nodes WHERE id = ?"
    )
    .bind(&node_id)
    .fetch_one(db)
    .await?;

    Ok(node)
}

/// Find entity by text (case-insensitive exact match)
///
/// # Arguments
/// * `db` - Database connection
/// * `text` - Entity text to search for
///
/// # Returns
/// First matching node, if any
pub async fn find_entity_by_text(
    db: &SqlitePool,
    text: &str,
) -> Result<Option<Node>> {
    // Search using JSON extract on properties
    // Note: This is not using an index yet (will add in Phase 2.5)
    let node: Option<Node> = sqlx::query_as(
        r#"
        SELECT * FROM nodes
        WHERE type IN ('person', 'project', 'location', 'app', 'tag')
          AND (
              json_extract(properties, '$.name') = ?
              OR json_extract(properties, '$.entity_text') = ?
          )
        LIMIT 1
        "#
    )
    .bind(text)
    .bind(text)
    .fetch_optional(db)
    .await?;

    Ok(node)
}

/// Find entity by text and label (more specific)
///
/// # Arguments
/// * `db` - Database connection
/// * `text` - Entity text
/// * `label` - Entity label (person, project, etc.)
///
/// # Returns
/// Matching node, if any
pub async fn find_entity_by_text_and_label(
    db: &SqlitePool,
    text: &str,
    label: &str,
) -> Result<Option<Node>> {
    let node_type = label_to_node_type(label);

    let node: Option<Node> = sqlx::query_as(
        r#"
        SELECT * FROM nodes
        WHERE type = ?
          AND (
              json_extract(properties, '$.name') = ?
              OR json_extract(properties, '$.entity_text') = ?
          )
        LIMIT 1
        "#
    )
    .bind(node_type.as_str())
    .bind(text)
    .bind(text)
    .fetch_optional(db)
    .await?;

    Ok(node)
}

/// Deduplicate entity (get or create)
///
/// If entity with this text and label already exists, return it.
/// Otherwise, create a new one.
///
/// # Arguments
/// * `db` - Database connection
/// * `text` - Entity text
/// * `label` - Entity label
/// * `confidence` - Extraction confidence
///
/// # Returns
/// Existing or newly created node
pub async fn deduplicate_entity(
    db: &SqlitePool,
    text: &str,
    label: &str,
    confidence: f32,
) -> Result<Node> {
    // Try to find existing entity
    if let Some(existing) = find_entity_by_text_and_label(db, text, label).await? {
        return Ok(existing);
    }

    // Not found, create new
    create_entity_node(db, text, label, confidence).await
}

/// Link a resource (file) to an entity via REFERENCES edge
///
/// # Arguments
/// * `db` - Database connection
/// * `file_id` - Source node ID (file)
/// * `entity_id` - Target node ID (entity)
/// * `confidence` - Confidence of this reference
///
/// # Returns
/// Created edge
pub async fn link_resource_to_entity(
    db: &SqlitePool,
    file_id: &str,
    entity_id: &str,
    confidence: f32,
) -> Result<Edge> {
    let now = Utc::now().to_rfc3339();

    // Check if edge already exists
    let existing: Option<(i64,)> = sqlx::query_as(
        r#"
        SELECT id FROM edges
        WHERE source_id = ? AND target_id = ? AND type = 'REFERENCES'
        "#
    )
    .bind(file_id)
    .bind(entity_id)
    .fetch_optional(db)
    .await?;

    if let Some((edge_id,)) = existing {
        // Edge exists, update weight (take maximum confidence)
        sqlx::query(
            r#"
            UPDATE edges
            SET weight = MAX(weight, ?)
            WHERE id = ?
            "#
        )
        .bind(confidence)
        .bind(edge_id)
        .execute(db)
        .await?;

        // Fetch and return updated edge
        let edge: Edge = sqlx::query_as(
            "SELECT * FROM edges WHERE id = ?"
        )
        .bind(edge_id)
        .fetch_one(db)
        .await?;

        return Ok(edge);
    }

    // Create new edge
    sqlx::query(
        r#"
        INSERT INTO edges (source_id, target_id, type, weight, created_at)
        VALUES (?, ?, 'REFERENCES', ?, ?)
        "#
    )
    .bind(file_id)
    .bind(entity_id)
    .bind(confidence)
    .bind(&now)
    .execute(db)
    .await
    .context("Failed to create REFERENCES edge")?;

    // Fetch the created edge
    let edge: Edge = sqlx::query_as(
        r#"
        SELECT * FROM edges
        WHERE source_id = ? AND target_id = ? AND type = 'REFERENCES'
        ORDER BY created_at DESC
        LIMIT 1
        "#
    )
    .bind(file_id)
    .bind(entity_id)
    .fetch_one(db)
    .await?;

    Ok(edge)
}

/// Get all entities referenced by a file
///
/// # Arguments
/// * `db` - Database connection
/// * `file_id` - File node ID
///
/// # Returns
/// List of entity nodes referenced by this file
pub async fn get_entities_for_file(
    db: &SqlitePool,
    file_id: &str,
) -> Result<Vec<Node>> {
    let nodes: Vec<Node> = sqlx::query_as(
        r#"
        SELECT n.* FROM nodes n
        JOIN edges e ON e.target_id = n.id
        WHERE e.source_id = ?
          AND e.type = 'REFERENCES'
        ORDER BY e.weight DESC
        "#
    )
    .bind(file_id)
    .fetch_all(db)
    .await?;

    Ok(nodes)
}

/// Get all files that reference an entity
///
/// # Arguments
/// * `db` - Database connection
/// * `entity_id` - Entity node ID
///
/// # Returns
/// List of file nodes that reference this entity
pub async fn get_files_for_entity(
    db: &SqlitePool,
    entity_id: &str,
) -> Result<Vec<Node>> {
    let nodes: Vec<Node> = sqlx::query_as(
        r#"
        SELECT n.* FROM nodes n
        JOIN edges e ON e.source_id = n.id
        WHERE e.target_id = ?
          AND e.type = 'REFERENCES'
          AND n.type = 'file'
        ORDER BY e.weight DESC
        "#
    )
    .bind(entity_id)
    .fetch_all(db)
    .await?;

    Ok(nodes)
}

/// Process extracted entities and link to file
///
/// This is the main entry point for entity extraction pipeline.
/// It:
/// 1. Deduplicates entities (get or create)
/// 2. Links entities to the file
/// 3. Returns list of entity nodes created/found
///
/// # Arguments
/// * `db` - Database connection
/// * `file_id` - File node ID
/// * `entities` - Extracted entities from GLiNER
///
/// # Returns
/// List of entity nodes
pub async fn process_entities_for_file(
    db: &SqlitePool,
    file_id: &str,
    entities: &[Entity],
) -> Result<Vec<Node>> {
    let mut entity_nodes = Vec::new();

    for entity in entities {
        // Deduplicate entity (get or create)
        let node = deduplicate_entity(
            db,
            &entity.text,
            &entity.label,
            entity.confidence,
        ).await?;

        // Link file to entity
        link_resource_to_entity(
            db,
            file_id,
            &node.id,
            entity.confidence,
        ).await?;

        entity_nodes.push(node);
    }

    Ok(entity_nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn setup_test_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // Create tables
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
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                type TEXT NOT NULL,
                weight REAL DEFAULT 1.0,
                properties TEXT,
                created_at TEXT NOT NULL
            )
            "#
        )
        .execute(&db)
        .await
        .unwrap();

        db
    }

    #[tokio::test]
    async fn test_create_entity_node() {
        let db = setup_test_db().await;

        let node = create_entity_node(&db, "Alice", "person", 0.95).await.unwrap();

        assert_eq!(node.r#type, NodeType::Person);
        assert!(node.properties.contains("Alice"));
    }

    #[tokio::test]
    async fn test_find_entity_by_text() {
        let db = setup_test_db().await;

        // Create entity
        let created = create_entity_node(&db, "Bob", "person", 0.9).await.unwrap();

        // Find it
        let found = find_entity_by_text(&db, "Bob").await.unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().id, created.id);

        // Try to find non-existent
        let not_found = find_entity_by_text(&db, "Charlie").await.unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_deduplicate_entity() {
        let db = setup_test_db().await;

        // Create first time
        let first = deduplicate_entity(&db, "Project Mars", "project", 0.8).await.unwrap();

        // Create again (should return same)
        let second = deduplicate_entity(&db, "Project Mars", "project", 0.85).await.unwrap();

        assert_eq!(first.id, second.id);

        // Count entities
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM nodes WHERE type = 'project'"
        )
        .fetch_one(&db)
        .await
        .unwrap();

        assert_eq!(count.0, 1);  // Only one entity created
    }

    #[tokio::test]
    async fn test_link_resource_to_entity() {
        let db = setup_test_db().await;

        // Create file node
        let file_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', '{}', ?, ?)"
        )
        .bind(&file_id)
        .bind(&now)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        // Create entity
        let entity = create_entity_node(&db, "Alice", "person", 0.95).await.unwrap();

        // Link them
        let edge = link_resource_to_entity(&db, &file_id, &entity.id, 0.95).await.unwrap();

        assert_eq!(edge.source_id, file_id);
        assert_eq!(edge.target_id, entity.id);
        assert_eq!(edge.r#type, EdgeType::References);
        assert!((edge.weight - 0.95).abs() < 0.01);
    }

    #[tokio::test]
    async fn test_get_entities_for_file() {
        let db = setup_test_db().await;

        // Create file
        let file_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', '{}', ?, ?)"
        )
        .bind(&file_id)
        .bind(&now)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        // Create and link entities
        let alice = create_entity_node(&db, "Alice", "person", 0.95).await.unwrap();
        let bob = create_entity_node(&db, "Bob", "person", 0.9).await.unwrap();

        link_resource_to_entity(&db, &file_id, &alice.id, 0.95).await.unwrap();
        link_resource_to_entity(&db, &file_id, &bob.id, 0.9).await.unwrap();

        // Get entities for file
        let entities = get_entities_for_file(&db, &file_id).await.unwrap();

        assert_eq!(entities.len(), 2);
        // Should be ordered by weight (Alice first)
        assert_eq!(entities[0].id, alice.id);
        assert_eq!(entities[1].id, bob.id);
    }

    #[tokio::test]
    async fn test_get_files_for_entity() {
        let db = setup_test_db().await;

        // Create entity
        let entity = create_entity_node(&db, "Alice", "person", 0.95).await.unwrap();

        // Create files
        let now = Utc::now().to_rfc3339();
        let file1_id = Uuid::new_v4().to_string();
        let file2_id = Uuid::new_v4().to_string();

        sqlx::query(
            "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', '{}', ?, ?)"
        )
        .bind(&file1_id)
        .bind(&now)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', '{}', ?, ?)"
        )
        .bind(&file2_id)
        .bind(&now)
        .bind(&now)
        .execute(&db)
        .await
        .unwrap();

        // Link files to entity
        link_resource_to_entity(&db, &file1_id, &entity.id, 0.95).await.unwrap();
        link_resource_to_entity(&db, &file2_id, &entity.id, 0.9).await.unwrap();

        // Get files for entity
        let files = get_files_for_entity(&db, &entity.id).await.unwrap();

        assert_eq!(files.len(), 2);
    }
}
