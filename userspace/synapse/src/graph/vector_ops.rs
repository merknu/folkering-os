//! Vector operations for semantic search using sqlite-vec.
//!
//! This module provides vector similarity search capabilities:
//! - Insert embeddings into vector index
//! - k-NN search for similar vectors
//! - Manage vector storage lifecycle
//!
//! Uses sqlite-vec extension for fast SIMD-optimized vector search.

use crate::models::Node;
use crate::neural::EMBEDDING_DIM;
use sqlx::SqlitePool;
use anyhow::{Result, Context, bail};
use serde_json;

/// Insert or update embedding for a node
///
/// # Arguments
///
/// * `db` - Database connection
/// * `node_id` - Node ID to associate with embedding
/// * `embedding` - 384-dimensional embedding vector
///
/// # Returns
///
/// vec_rowid that was inserted/updated
///
/// # Errors
///
/// Returns error if:
/// - Embedding dimension is not 384
/// - Database operation fails
pub async fn insert_embedding(
    db: &SqlitePool,
    node_id: &str,
    embedding: &[f32],
) -> Result<i64> {
    // Validate dimension
    if embedding.len() != EMBEDDING_DIM {
        bail!(
            "Invalid embedding dimension: expected {}, got {}",
            EMBEDDING_DIM,
            embedding.len()
        );
    }

    // Serialize embedding to JSON array
    let embedding_json = serde_json::to_string(embedding)?;

    // Check if node already has embedding
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT vec_rowid FROM node_embeddings WHERE node_id = ?"
    )
    .bind(node_id)
    .fetch_optional(db)
    .await?;

    if let Some((vec_rowid,)) = existing {
        // Update existing embedding
        sqlx::query(
            "UPDATE vec_nodes SET embedding = ? WHERE rowid = ?"
        )
        .bind(&embedding_json)
        .bind(vec_rowid)
        .execute(db)
        .await
        .context("Failed to update embedding in vec_nodes")?;

        Ok(vec_rowid)
    } else {
        // Insert new embedding into vec_nodes
        let result = sqlx::query(
            "INSERT INTO vec_nodes(embedding) VALUES (?)"
        )
        .bind(&embedding_json)
        .execute(db)
        .await
        .context("Failed to insert into vec_nodes")?;

        let vec_rowid = result.last_insert_rowid();

        // Create mapping in node_embeddings
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO node_embeddings(node_id, vec_rowid, created_at) VALUES (?, ?, ?)"
        )
        .bind(node_id)
        .bind(vec_rowid)
        .bind(&now)
        .execute(db)
        .await
        .context("Failed to create node embedding mapping")?;

        Ok(vec_rowid)
    }
}

/// Search for similar nodes using k-NN vector search
///
/// # Arguments
///
/// * `db` - Database connection
/// * `query_embedding` - Query embedding (384 dimensions)
/// * `k` - Number of nearest neighbors to return
///
/// # Returns
///
/// List of (Node, similarity_score) tuples, ordered by similarity (descending)
///
/// # Errors
///
/// Returns error if:
/// - Query embedding dimension is not 384
/// - Database operation fails
///
/// # Example
///
/// ```no_run
/// # use synapse::graph::vector_ops;
/// let query = vec![0.1; 384];
/// let results = vector_ops::search_similar(&db, &query, 10).await?;
///
/// for (node, similarity) in results {
///     println!("{}: {:.4}", node.id, similarity);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn search_similar(
    db: &SqlitePool,
    query_embedding: &[f32],
    k: usize,
) -> Result<Vec<(Node, f32)>> {
    // Validate dimension
    if query_embedding.len() != EMBEDDING_DIM {
        bail!(
            "Invalid query embedding dimension: expected {}, got {}",
            EMBEDDING_DIM,
            query_embedding.len()
        );
    }

    // Validate k
    if k == 0 {
        bail!("k must be at least 1");
    }

    // Serialize query embedding
    let embedding_json = serde_json::to_string(query_embedding)?;

    // Perform k-NN search using sqlite-vec
    // Note: distance in sqlite-vec is actually 1 - cosine_similarity
    // So we convert: similarity = 1 - distance
    let results: Vec<(String, f32)> = sqlx::query_as(
        r#"
        SELECT
            ne.node_id,
            (1.0 - v.distance) as similarity
        FROM vec_nodes v
        JOIN node_embeddings ne ON ne.vec_rowid = v.rowid
        WHERE v.embedding MATCH ?
          AND v.k = ?
        ORDER BY v.distance
        "#
    )
    .bind(&embedding_json)
    .bind(k as i64)
    .fetch_all(db)
    .await
    .context("Vector search query failed")?;

    // Fetch full node objects
    let mut nodes_with_similarity = Vec::new();

    for (node_id, similarity) in results {
        // Fetch node
        let node: Option<Node> = sqlx::query_as(
            "SELECT * FROM nodes WHERE id = ?"
        )
        .bind(&node_id)
        .fetch_optional(db)
        .await?;

        if let Some(node) = node {
            nodes_with_similarity.push((node, similarity));
        }
    }

    Ok(nodes_with_similarity)
}

/// Get embedding for a node (if exists)
///
/// # Arguments
///
/// * `db` - Database connection
/// * `node_id` - Node ID
///
/// # Returns
///
/// Optional 384-dimensional embedding vector
pub async fn get_embedding(
    db: &SqlitePool,
    node_id: &str,
) -> Result<Option<Vec<f32>>> {
    // Get vec_rowid
    let vec_rowid: Option<(i64,)> = sqlx::query_as(
        "SELECT vec_rowid FROM node_embeddings WHERE node_id = ?"
    )
    .bind(node_id)
    .fetch_optional(db)
    .await?;

    let Some((vec_rowid,)) = vec_rowid else {
        return Ok(None);
    };

    // Fetch embedding from vec_nodes
    let embedding_json: (String,) = sqlx::query_as(
        "SELECT embedding FROM vec_nodes WHERE rowid = ?"
    )
    .bind(vec_rowid)
    .fetch_one(db)
    .await?;

    // Deserialize
    let embedding: Vec<f32> = serde_json::from_str(&embedding_json.0)?;

    Ok(Some(embedding))
}

/// Delete embedding for a node
///
/// # Arguments
///
/// * `db` - Database connection
/// * `node_id` - Node ID
///
/// # Returns
///
/// True if embedding was deleted, false if node had no embedding
pub async fn delete_embedding(
    db: &SqlitePool,
    node_id: &str,
) -> Result<bool> {
    // Get vec_rowid
    let vec_rowid: Option<(i64,)> = sqlx::query_as(
        "SELECT vec_rowid FROM node_embeddings WHERE node_id = ?"
    )
    .bind(node_id)
    .fetch_optional(db)
    .await?;

    let Some((vec_rowid,)) = vec_rowid else {
        return Ok(false);
    };

    // Delete from node_embeddings (mapping)
    sqlx::query("DELETE FROM node_embeddings WHERE node_id = ?")
        .bind(node_id)
        .execute(db)
        .await?;

    // Delete from vec_nodes (vector table)
    sqlx::query("DELETE FROM vec_nodes WHERE rowid = ?")
        .bind(vec_rowid)
        .execute(db)
        .await?;

    Ok(true)
}

/// Count total embeddings in database
///
/// # Arguments
///
/// * `db` - Database connection
///
/// # Returns
///
/// Number of embeddings stored
pub async fn count_embeddings(db: &SqlitePool) -> Result<i64> {
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM node_embeddings"
    )
    .fetch_one(db)
    .await?;

    Ok(count.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use uuid::Uuid;
    use chrono::Utc;

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

        // Note: In real usage, vec_nodes would be a virtual table
        // For testing without sqlite-vec extension, we use a regular table
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

    async fn create_test_node(db: &SqlitePool, node_id: &str) {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', '{}', ?, ?)"
        )
        .bind(node_id)
        .bind(&now)
        .bind(&now)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_insert_embedding() {
        let db = setup_test_db().await;
        let node_id = Uuid::new_v4().to_string();
        create_test_node(&db, &node_id).await;

        let embedding = vec![0.1; EMBEDDING_DIM];
        let vec_rowid = insert_embedding(&db, &node_id, &embedding).await.unwrap();

        assert!(vec_rowid > 0);

        // Verify mapping exists
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM node_embeddings WHERE node_id = ?"
        )
        .bind(&node_id)
        .fetch_one(&db)
        .await
        .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_update_embedding() {
        let db = setup_test_db().await;
        let node_id = Uuid::new_v4().to_string();
        create_test_node(&db, &node_id).await;

        // Insert first embedding
        let embedding1 = vec![0.1; EMBEDDING_DIM];
        let vec_rowid1 = insert_embedding(&db, &node_id, &embedding1).await.unwrap();

        // Update with new embedding
        let embedding2 = vec![0.2; EMBEDDING_DIM];
        let vec_rowid2 = insert_embedding(&db, &node_id, &embedding2).await.unwrap();

        // Should reuse same vec_rowid
        assert_eq!(vec_rowid1, vec_rowid2);

        // Should still have only one mapping
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM node_embeddings WHERE node_id = ?"
        )
        .bind(&node_id)
        .fetch_one(&db)
        .await
        .unwrap();

        assert_eq!(count.0, 1);
    }

    #[tokio::test]
    async fn test_get_embedding() {
        let db = setup_test_db().await;
        let node_id = Uuid::new_v4().to_string();
        create_test_node(&db, &node_id).await;

        let embedding = vec![0.5; EMBEDDING_DIM];
        insert_embedding(&db, &node_id, &embedding).await.unwrap();

        let retrieved = get_embedding(&db, &node_id).await.unwrap();
        assert!(retrieved.is_some());

        let retrieved_emb = retrieved.unwrap();
        assert_eq!(retrieved_emb.len(), EMBEDDING_DIM);
        assert!((retrieved_emb[0] - 0.5).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_get_nonexistent_embedding() {
        let db = setup_test_db().await;
        let node_id = "nonexistent";

        let retrieved = get_embedding(&db, node_id).await.unwrap();
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_delete_embedding() {
        let db = setup_test_db().await;
        let node_id = Uuid::new_v4().to_string();
        create_test_node(&db, &node_id).await;

        let embedding = vec![0.3; EMBEDDING_DIM];
        insert_embedding(&db, &node_id, &embedding).await.unwrap();

        // Delete
        let deleted = delete_embedding(&db, &node_id).await.unwrap();
        assert!(deleted);

        // Verify deleted
        let retrieved = get_embedding(&db, &node_id).await.unwrap();
        assert!(retrieved.is_none());

        // Delete again should return false
        let deleted_again = delete_embedding(&db, &node_id).await.unwrap();
        assert!(!deleted_again);
    }

    #[tokio::test]
    async fn test_count_embeddings() {
        let db = setup_test_db().await;

        // Initially 0
        let count = count_embeddings(&db).await.unwrap();
        assert_eq!(count, 0);

        // Insert 3 embeddings
        for i in 0..3 {
            let node_id = format!("node-{}", i);
            create_test_node(&db, &node_id).await;
            let embedding = vec![0.1; EMBEDDING_DIM];
            insert_embedding(&db, &node_id, &embedding).await.unwrap();
        }

        let count = count_embeddings(&db).await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_invalid_dimension() {
        let db = setup_test_db().await;
        let node_id = "test-node";

        let wrong_embedding = vec![0.1; 128]; // Wrong dimension
        let result = insert_embedding(&db, node_id, &wrong_embedding).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid embedding dimension"));
    }
}
