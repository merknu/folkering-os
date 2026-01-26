//! Semantic query methods - High-level API for intelligent search
//!
//! This module provides convenient, semantic search functions that build on:
//! - Hybrid search (RRF)
//! - Vector similarity search
//! - Entity graph traversal
//!
//! These are the primary user-facing query APIs.

use crate::models::{Node, NodeType};
use crate::graph::vector_ops;
use crate::neural::EmbeddingService;
use crate::query::hybrid_search;
use sqlx::SqlitePool;
use anyhow::{Result, Context};

/// Find documents similar to a given document
///
/// Uses vector similarity to find semantically related documents.
///
/// # Arguments
///
/// * `db` - Database connection
/// * `embedder` - Embedding service
/// * `file_id` - Source file node ID
/// * `threshold` - Similarity threshold (0.0-1.0), typically 0.5-0.7
/// * `limit` - Maximum results
///
/// # Returns
///
/// List of similar documents ordered by similarity (best first)
///
/// # Example
///
/// ```no_run
/// # use synapse::query::semantic;
/// let similar = semantic::find_similar(&db, &embedder, "design_doc.md", 0.6, 10).await?;
/// for node in similar {
///     println!("Similar file: {}", node.id);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn find_similar(
    db: &SqlitePool,
    _embedder: &EmbeddingService,
    file_id: &str,
    threshold: f32,
    limit: usize,
) -> Result<Vec<Node>> {
    // Get embedding for source file
    let embedding = vector_ops::get_embedding(db, file_id)
        .await
        .context("Failed to get source file embedding")?
        .ok_or_else(|| anyhow::anyhow!("File {} has no embedding", file_id))?;

    // Search for similar documents
    let results = vector_ops::search_similar(db, &embedding, limit * 2).await?;

    // Filter by threshold and exclude source file
    let similar: Vec<Node> = results
        .into_iter()
        .filter(|(node, sim)| node.id != file_id && *sim >= threshold)
        .take(limit)
        .map(|(node, _sim)| node)
        .collect();

    Ok(similar)
}

/// Find files that mention a specific entity
///
/// Traverses the entity graph to find all files that reference an entity.
///
/// # Arguments
///
/// * `db` - Database connection
/// * `entity_text` - Entity text (e.g., "Alice", "Project Mars")
/// * `entity_label` - Entity type (e.g., "person", "project")
///
/// # Returns
///
/// List of file nodes that mention this entity
///
/// # Example
///
/// ```no_run
/// # use synapse::query::semantic;
/// let files = semantic::find_files_mentioning_entity(&db, "Alice", "person").await?;
/// for file in files {
///     println!("File mentioning Alice: {}", file.id);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn find_files_mentioning_entity(
    db: &SqlitePool,
    entity_text: &str,
    entity_label: &str,
) -> Result<Vec<Node>> {
    // Find entity node
    let entity_node = find_entity_node(db, entity_text, entity_label).await?;

    let entity_id = match entity_node {
        Some(node) => node.id,
        None => return Ok(Vec::new()), // Entity not found
    };

    // Find files that reference this entity
    // Follow REFERENCES edges backwards (file --REFERENCES--> entity)
    let files = sqlx::query_as::<_, Node>(
        r#"
        SELECT DISTINCT n.*
        FROM nodes n
        JOIN edges e ON n.id = e.source_id
        WHERE e.target_id = ?
          AND e.type = 'REFERENCES'
          AND n.type = 'file'
        ORDER BY n.updated_at DESC
        "#
    )
    .bind(&entity_id)
    .fetch_all(db)
    .await
    .context("Failed to query files mentioning entity")?;

    Ok(files)
}

/// Find files about a concept (semantic search)
///
/// Uses embeddings to find documents semantically related to a concept.
///
/// # Arguments
///
/// * `db` - Database connection
/// * `embedder` - Embedding service
/// * `concept_text` - Concept description (e.g., "machine learning")
/// * `threshold` - Similarity threshold (0.0-1.0)
/// * `limit` - Maximum results
///
/// # Returns
///
/// List of relevant documents ordered by relevance
///
/// # Example
///
/// ```no_run
/// # use synapse::query::semantic;
/// let files = semantic::find_files_about(&db, &embedder, "machine learning", 0.5, 10).await?;
/// for file in files {
///     println!("File about ML: {}", file.id);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn find_files_about(
    db: &SqlitePool,
    embedder: &EmbeddingService,
    concept_text: &str,
    threshold: f32,
    limit: usize,
) -> Result<Vec<Node>> {
    // Generate embedding for concept
    let concept_embedding = embedder.generate(concept_text)
        .context("Failed to generate concept embedding")?;

    // Vector search
    let results = vector_ops::search_similar(db, &concept_embedding, limit * 2).await?;

    // Filter by threshold
    let relevant: Vec<Node> = results
        .into_iter()
        .filter(|(_node, sim)| *sim >= threshold)
        .take(limit)
        .map(|(node, _sim)| node)
        .collect();

    Ok(relevant)
}

/// Find entities related to a given entity (co-occurrence analysis)
///
/// Finds other entities that appear in the same files as the given entity.
///
/// # Arguments
///
/// * `db` - Database connection
/// * `entity_text` - Entity text
/// * `entity_label` - Entity type
/// * `limit` - Maximum results
///
/// # Returns
///
/// List of related entity nodes ordered by co-occurrence frequency
///
/// # Example
///
/// ```no_run
/// # use synapse::query::semantic;
/// let related = semantic::find_related_entities(&db, "Alice", "person", 10).await?;
/// for entity in related {
///     println!("Related entity: {}", entity.id);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn find_related_entities(
    db: &SqlitePool,
    entity_text: &str,
    entity_label: &str,
    limit: usize,
) -> Result<Vec<Node>> {
    // Find source entity
    let entity_node = find_entity_node(db, entity_text, entity_label).await?;

    let entity_id = match entity_node {
        Some(node) => node.id,
        None => return Ok(Vec::new()),
    };

    // Find files that mention this entity
    let files = find_files_mentioning_entity(db, entity_text, entity_label).await?;

    if files.is_empty() {
        return Ok(Vec::new());
    }

    // Find other entities mentioned in those files
    // Build IN clause for file IDs
    let file_ids: Vec<String> = files.iter().map(|f| f.id.clone()).collect();
    let placeholders = vec!["?"; file_ids.len()].join(",");

    let query = format!(
        r#"
        SELECT DISTINCT e_node.*, COUNT(*) as mentions
        FROM nodes e_node
        JOIN edges e ON e_node.id = e.target_id
        WHERE e.source_id IN ({})
          AND e.type = 'REFERENCES'
          AND e_node.id != ?
          AND e_node.type IN ('person', 'project', 'concept', 'location', 'organization')
        GROUP BY e_node.id
        ORDER BY mentions DESC
        LIMIT ?
        "#,
        placeholders
    );

    let mut query_builder = sqlx::query_as::<_, Node>(&query);
    for id in &file_ids {
        query_builder = query_builder.bind(id);
    }
    query_builder = query_builder.bind(&entity_id); // Exclude source entity
    query_builder = query_builder.bind(limit as i64);

    let related = query_builder
        .fetch_all(db)
        .await
        .context("Failed to find related entities")?;

    Ok(related)
}

/// Hybrid search with entity context
///
/// Combines hybrid search with entity awareness for richer results.
///
/// # Arguments
///
/// * `db` - Database connection
/// * `embedder` - Embedding service
/// * `query` - Search query
/// * `limit` - Maximum results
///
/// # Returns
///
/// Hybrid search results with entity context
///
/// # Example
///
/// ```no_run
/// # use synapse::query::semantic;
/// let results = semantic::search_with_context(&db, &embedder, "machine learning", 10).await?;
/// for result in results {
///     println!("{}: score={:.4}", result.node.id, result.score);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn search_with_context(
    db: &SqlitePool,
    embedder: &EmbeddingService,
    query: &str,
    limit: usize,
) -> Result<Vec<hybrid_search::HybridResult>> {
    // Use hybrid search (already combines FTS + vector)
    hybrid_search::search(db, embedder, query, limit).await
}

/// Helper: Find entity node by text and label
async fn find_entity_node(
    db: &SqlitePool,
    entity_text: &str,
    entity_label: &str,
) -> Result<Option<Node>> {
    let node = sqlx::query_as::<_, Node>(
        r#"
        SELECT *
        FROM nodes
        WHERE type = ?
          AND json_extract(properties, '$.entity_text') = ?
        LIMIT 1
        "#
    )
    .bind(entity_label)
    .bind(entity_text)
    .fetch_optional(db)
    .await
    .context("Failed to find entity node")?;

    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use chrono::Utc;

    async fn setup_test_db() -> SqlitePool {
        let db = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        // Create schema
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
                created_at TEXT NOT NULL,
                FOREIGN KEY (source_id) REFERENCES nodes(id) ON DELETE CASCADE,
                FOREIGN KEY (target_id) REFERENCES nodes(id) ON DELETE CASCADE
            )
            "#
        )
        .execute(&db)
        .await
        .unwrap();

        // Vector tables
        sqlx::query(
            r#"
            CREATE TABLE vec_nodes (
                rowid INTEGER PRIMARY KEY AUTOINCREMENT,
                embedding TEXT NOT NULL
            )
            "#
        )
        .execute(&db)
        .await
        .unwrap();

        sqlx::query(
            r#"
            CREATE TABLE node_embeddings (
                node_id TEXT PRIMARY KEY,
                vec_rowid INTEGER NOT NULL,
                created_at TEXT NOT NULL
            )
            "#
        )
        .execute(&db)
        .await
        .unwrap();

        db
    }

    async fn create_test_node(db: &SqlitePool, id: &str, node_type: &str, props: serde_json::Value) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, ?, ?, ?, ?)"
        )
        .bind(id)
        .bind(node_type)
        .bind(props.to_string())
        .bind(&now)
        .bind(&now)
        .execute(db)
        .await
        .unwrap();
    }

    async fn create_edge(db: &SqlitePool, source: &str, target: &str, edge_type: &str) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO edges (source_id, target_id, type, created_at) VALUES (?, ?, ?, ?)"
        )
        .bind(source)
        .bind(target)
        .bind(edge_type)
        .bind(&now)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_find_files_mentioning_entity() {
        let db = setup_test_db().await;

        // Create entity
        create_test_node(
            &db,
            "entity_alice",
            "person",
            serde_json::json!({"entity_text": "Alice", "label": "person"})
        ).await;

        // Create files
        create_test_node(&db, "file1", "file", serde_json::json!({"name": "team.md"})).await;
        create_test_node(&db, "file2", "file", serde_json::json!({"name": "project.md"})).await;

        // Create REFERENCES edges
        create_edge(&db, "file1", "entity_alice", "REFERENCES").await;
        create_edge(&db, "file2", "entity_alice", "REFERENCES").await;

        // Query
        let files = find_files_mentioning_entity(&db, "Alice", "person").await.unwrap();

        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| f.id == "file1"));
        assert!(files.iter().any(|f| f.id == "file2"));
    }

    #[tokio::test]
    async fn test_find_related_entities() {
        let db = setup_test_db().await;

        // Create entities
        create_test_node(&db, "alice", "person", serde_json::json!({"entity_text": "Alice"})).await;
        create_test_node(&db, "bob", "person", serde_json::json!({"entity_text": "Bob"})).await;
        create_test_node(&db, "project_mars", "project", serde_json::json!({"entity_text": "Project Mars"})).await;

        // Create file
        create_test_node(&db, "file1", "file", serde_json::json!({"name": "team.md"})).await;

        // File references all entities
        create_edge(&db, "file1", "alice", "REFERENCES").await;
        create_edge(&db, "file1", "bob", "REFERENCES").await;
        create_edge(&db, "file1", "project_mars", "REFERENCES").await;

        // Query: entities related to Alice
        let related = find_related_entities(&db, "Alice", "person", 10).await.unwrap();

        // Should find Bob and Project Mars (not Alice herself)
        assert_eq!(related.len(), 2);
        assert!(related.iter().any(|e| e.id == "bob"));
        assert!(related.iter().any(|e| e.id == "project_mars"));
        assert!(!related.iter().any(|e| e.id == "alice"));
    }

    #[tokio::test]
    async fn test_find_entity_node() {
        let db = setup_test_db().await;

        create_test_node(
            &db,
            "entity_alice",
            "person",
            serde_json::json!({"entity_text": "Alice", "label": "person"})
        ).await;

        let node = find_entity_node(&db, "Alice", "person").await.unwrap();
        assert!(node.is_some());
        assert_eq!(node.unwrap().id, "entity_alice");

        let not_found = find_entity_node(&db, "Bob", "person").await.unwrap();
        assert!(not_found.is_none());
    }
}
