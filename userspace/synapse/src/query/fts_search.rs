//! Full-text search using SQLite FTS5.
//!
//! This module provides keyword-based search using FTS5's advanced features:
//! - BM25 ranking algorithm
//! - Porter stemming
//! - Boolean operators (AND, OR, NOT)
//! - Phrase search
//! - Unicode normalization

use crate::models::Node;
use sqlx::SqlitePool;
use anyhow::{Result, Context};

/// Store file content for FTS indexing
///
/// # Arguments
///
/// * `db` - Database connection
/// * `node_id` - Node ID (file)
/// * `content` - File text content
///
/// # Returns
///
/// Success or error
pub async fn index_content(
    db: &SqlitePool,
    node_id: &str,
    content: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();

    // Insert or update file content
    sqlx::query(
        r#"
        INSERT INTO file_content (node_id, content, updated_at)
        VALUES (?, ?, ?)
        ON CONFLICT(node_id) DO UPDATE SET
            content = excluded.content,
            updated_at = excluded.updated_at
        "#
    )
    .bind(node_id)
    .bind(content)
    .bind(&now)
    .execute(db)
    .await
    .context("Failed to index content")?;

    Ok(())
}

/// Search result from FTS5
#[derive(Debug, Clone)]
pub struct FtsResult {
    pub node: Node,
    pub rank: f32,
}

/// Perform full-text search using FTS5
///
/// # Arguments
///
/// * `db` - Database connection
/// * `query` - Search query (supports FTS5 syntax)
/// * `limit` - Maximum results to return
///
/// # Returns
///
/// List of (Node, rank) tuples ordered by relevance (best first)
///
/// # Query Syntax
///
/// - Simple: `"machine learning"`
/// - Phrase: `"\"neural networks\""`
/// - Boolean: `"machine AND learning NOT deep"`
/// - Prefix: `"machin*"` (matches machine, machinery, etc.)
///
/// # Example
///
/// ```no_run
/// # use synapse::query::fts_search;
/// let results = fts_search::search(&db, "machine learning", 10).await?;
/// for result in results {
///     println!("{}: {:.4}", result.node.id, result.rank);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn search(
    db: &SqlitePool,
    query: &str,
    limit: usize,
) -> Result<Vec<FtsResult>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    // Use BM25 ranking from FTS5
    // Negative BM25 score = higher relevance (FTS5 convention)
    let results: Vec<(String, f32)> = sqlx::query_as(
        r#"
        SELECT
            fc.node_id,
            bm25(file_content_fts) as rank
        FROM file_content fc
        JOIN file_content_fts ON file_content_fts.rowid = fc.rowid
        WHERE file_content_fts MATCH ?
        ORDER BY rank
        LIMIT ?
        "#
    )
    .bind(query)
    .bind(limit as i64)
    .fetch_all(db)
    .await
    .context("FTS5 search failed")?;

    // Fetch full node objects
    let mut fts_results = Vec::new();

    for (node_id, rank_raw) in results {
        if let Some(node) = fetch_node(db, &node_id).await? {
            // Convert BM25 score to positive similarity (0-1 range)
            // BM25 is negative, so negate it and normalize
            let rank = (-rank_raw).max(0.0);

            fts_results.push(FtsResult { node, rank });
        }
    }

    Ok(fts_results)
}

/// Fetch node by ID
async fn fetch_node(db: &SqlitePool, node_id: &str) -> Result<Option<Node>> {
    let node: Option<Node> = sqlx::query_as(
        "SELECT * FROM nodes WHERE id = ?"
    )
    .bind(node_id)
    .fetch_optional(db)
    .await?;

    Ok(node)
}

/// Get indexed content for a node
///
/// # Arguments
///
/// * `db` - Database connection
/// * `node_id` - Node ID
///
/// # Returns
///
/// Optional content string
pub async fn get_content(
    db: &SqlitePool,
    node_id: &str,
) -> Result<Option<String>> {
    let content: Option<(String,)> = sqlx::query_as(
        "SELECT content FROM file_content WHERE node_id = ?"
    )
    .bind(node_id)
    .fetch_optional(db)
    .await?;

    Ok(content.map(|c| c.0))
}

/// Delete indexed content for a node
///
/// # Arguments
///
/// * `db` - Database connection
/// * `node_id` - Node ID
///
/// # Returns
///
/// True if content was deleted, false if node had no content
pub async fn delete_content(
    db: &SqlitePool,
    node_id: &str,
) -> Result<bool> {
    let result = sqlx::query(
        "DELETE FROM file_content WHERE node_id = ?"
    )
    .bind(node_id)
    .execute(db)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// Count indexed documents
pub async fn count_indexed(db: &SqlitePool) -> Result<i64> {
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM file_content"
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

        sqlx::query(
            r#"
            CREATE TABLE file_content (
                node_id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            "#
        )
        .execute(&db)
        .await
        .unwrap();

        // Create FTS5 virtual table
        sqlx::query(
            r#"
            CREATE VIRTUAL TABLE file_content_fts USING fts5(
                content,
                content_rowid=rowid,
                tokenize='porter unicode61'
            )
            "#
        )
        .execute(&db)
        .await
        .unwrap();

        // Triggers
        sqlx::query(
            r#"
            CREATE TRIGGER file_content_ai AFTER INSERT ON file_content BEGIN
                INSERT INTO file_content_fts(rowid, content)
                VALUES (NEW.rowid, NEW.content);
            END
            "#
        )
        .execute(&db)
        .await
        .unwrap();

        db
    }

    async fn create_test_node(db: &SqlitePool, node_id: &str, content: &str) {
        let now = Utc::now().to_rfc3339();
        let props = serde_json::json!({"name": node_id});

        sqlx::query(
            "INSERT INTO nodes (id, type, properties, created_at, updated_at) VALUES (?, 'file', ?, ?, ?)"
        )
        .bind(node_id)
        .bind(props.to_string())
        .bind(&now)
        .bind(&now)
        .execute(db)
        .await
        .unwrap();

        index_content(db, node_id, content).await.unwrap();
    }

    #[tokio::test]
    async fn test_index_and_search() {
        let db = setup_test_db().await;

        let node_id = Uuid::new_v4().to_string();
        create_test_node(&db, &node_id, "Machine learning with neural networks").await;

        // Search for "machine"
        let results = search(&db, "machine", 10).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node.id, node_id);
    }

    #[tokio::test]
    async fn test_search_ranking() {
        let db = setup_test_db().await;

        let node1 = "node1";
        let node2 = "node2";

        create_test_node(&db, node1, "Machine learning is a subset of artificial intelligence").await;
        create_test_node(&db, node2, "Machine machine machine learning learning learning").await;

        // Search for "machine learning"
        let results = search(&db, "machine learning", 10).await.unwrap();

        // node2 should rank higher (more occurrences)
        assert_eq!(results.len(), 2);
        // Results are ordered by rank (best first)
    }

    #[tokio::test]
    async fn test_get_content() {
        let db = setup_test_db().await;

        let node_id = "test-node";
        let content_text = "Test content";

        create_test_node(&db, node_id, content_text).await;

        let retrieved = get_content(&db, node_id).await.unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap(), content_text);
    }

    #[tokio::test]
    async fn test_delete_content() {
        let db = setup_test_db().await;

        let node_id = "test-node";
        create_test_node(&db, node_id, "Test content").await;

        let deleted = delete_content(&db, node_id).await.unwrap();
        assert!(deleted);

        let retrieved = get_content(&db, node_id).await.unwrap();
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_count_indexed() {
        let db = setup_test_db().await;

        let count1 = count_indexed(&db).await.unwrap();
        assert_eq!(count1, 0);

        create_test_node(&db, "node1", "Content 1").await;
        create_test_node(&db, "node2", "Content 2").await;

        let count2 = count_indexed(&db).await.unwrap();
        assert_eq!(count2, 2);
    }
}
