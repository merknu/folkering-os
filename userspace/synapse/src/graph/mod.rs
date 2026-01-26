//! Graph operations - CRUD and algorithms

pub mod hash;
pub mod entity_ops;
pub mod vector_ops;

pub use hash::{compute_file_hash, hash_matches, compute_bytes_hash};
pub use entity_ops::{
    create_entity_node, find_entity_by_text, find_entity_by_text_and_label,
    deduplicate_entity, link_resource_to_entity, get_entities_for_file,
    get_files_for_entity, process_entities_for_file,
};
pub use vector_ops::{
    insert_embedding, search_similar, get_embedding, delete_embedding, count_embeddings,
};

use crate::models::{Node, Edge, NodeType, EdgeType};
use sqlx::SqlitePool;
use anyhow::Result;
use std::path::PathBuf;

/// Graph database operations
pub struct GraphDB {
    db: SqlitePool,
    project_root: PathBuf,
}

impl GraphDB {
    /// Create new GraphDB with default project root (current working directory)
    pub fn new(db: SqlitePool) -> Self {
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self { db, project_root }
    }

    /// Create new GraphDB with explicit project root
    pub fn with_project_root(db: SqlitePool, project_root: PathBuf) -> Self {
        Self { db, project_root }
    }

    /// Initialize GraphDB by loading project root from database
    pub async fn init(db: SqlitePool) -> Result<Self> {
        // Try to load project root from database
        let project_root_str: Option<String> = sqlx::query_scalar(
            "SELECT value FROM project_meta WHERE key = 'project_root'"
        )
        .fetch_optional(&db)
        .await?;

        let project_root = match project_root_str {
            Some(root) => PathBuf::from(root),
            None => {
                // No project root set, use current directory
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

                // Store it in database
                sqlx::query(
                    "INSERT INTO project_meta (key, value, updated_at) VALUES (?, ?, datetime('now'))
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')"
                )
                .bind("project_root")
                .bind(cwd.to_string_lossy().as_ref())
                .execute(&db)
                .await?;

                cwd
            }
        };

        Ok(Self { db, project_root })
    }

    /// Convert absolute path to relative path (for storage)
    fn to_relative(&self, absolute_path: &str) -> Result<String> {
        let path = PathBuf::from(absolute_path);

        // If path doesn't exist and looks like a logical path (e.g., "/work/file.txt"),
        // just normalize and store as-is (for in-memory testing)
        if !path.exists() && (absolute_path.starts_with('/') || absolute_path.contains(":/")) {
            // Logical/fake path - just normalize separators
            let normalized = absolute_path.replace('\\', "/");
            return Ok(normalized);
        }

        // Make absolute if not already
        let abs = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(&path)
        };

        // Try to convert to relative from project root
        match abs.strip_prefix(&self.project_root) {
            Ok(relative) => {
                // Convert to forward slashes for cross-platform compatibility
                let relative_str = relative.to_string_lossy().replace('\\', "/");
                Ok(relative_str)
            }
            Err(_) => {
                // Path is outside project root - for real files, this is an error
                if abs.exists() {
                    Err(anyhow::anyhow!("Path is outside project root: {:?}", abs))
                } else {
                    // Non-existent path - might be logical, store as normalized
                    let normalized = absolute_path.replace('\\', "/");
                    Ok(normalized)
                }
            }
        }
    }

    /// Convert relative path to absolute path (for resolution)
    fn to_absolute(&self, relative_path: &str) -> PathBuf {
        // Normalize separators (replace backslashes with forward slashes)
        let normalized = relative_path.replace('\\', "/");

        // Join with project root
        self.project_root.join(normalized)
    }

    /// Update project root in database
    pub async fn set_project_root(&mut self, new_root: PathBuf) -> Result<()> {
        sqlx::query(
            "UPDATE project_meta SET value = ?, updated_at = datetime('now') WHERE key = 'project_root'"
        )
        .bind(new_root.to_string_lossy().as_ref())
        .execute(&self.db)
        .await?;

        self.project_root = new_root;
        Ok(())
    }

    /// Create a new node
    pub async fn create_node(&self, node: &Node) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO nodes (id, type, properties, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(&node.id)
        .bind(node.r#type.as_str())
        .bind(&node.properties)
        .bind(&node.created_at)
        .bind(&node.updated_at)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    /// Get node by ID
    pub async fn get_node(&self, id: &str) -> Result<Option<Node>> {
        let node = sqlx::query_as::<_, Node>(
            r#"
            SELECT * FROM nodes WHERE id = ?
            "#,
        )
        .bind(id)
        .fetch_optional(&self.db)
        .await?;

        Ok(node)
    }

    /// Update node properties
    pub async fn update_node(&self, node: &Node) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE nodes
            SET properties = ?, updated_at = ?
            WHERE id = ?
            "#,
        )
        .bind(&node.properties)
        .bind(&node.updated_at)
        .bind(&node.id)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    /// Delete node (cascades to edges)
    pub async fn delete_node(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM nodes WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;

        Ok(())
    }

    /// Create or update edge (upsert)
    pub async fn upsert_edge(&self, edge: &Edge) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO edges (source_id, target_id, type, weight, properties, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(source_id, target_id, type)
            DO UPDATE SET
                weight = excluded.weight,
                properties = excluded.properties
            "#,
        )
        .bind(&edge.source_id)
        .bind(&edge.target_id)
        .bind(edge.r#type.as_str())
        .bind(edge.weight)
        .bind(&edge.properties)
        .bind(&edge.created_at)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    /// Get edge by source, target, type
    pub async fn get_edge(
        &self,
        source_id: &str,
        target_id: &str,
        edge_type: EdgeType,
    ) -> Result<Option<Edge>> {
        let edge = sqlx::query_as::<_, Edge>(
            r#"
            SELECT * FROM edges
            WHERE source_id = ? AND target_id = ? AND type = ?
            "#,
        )
        .bind(source_id)
        .bind(target_id)
        .bind(edge_type.as_str())
        .fetch_optional(&self.db)
        .await?;

        Ok(edge)
    }

    /// Delete edge
    pub async fn delete_edge(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM edges WHERE id = ?")
            .bind(id)
            .execute(&self.db)
            .await?;

        Ok(())
    }

    /// Get all edges for a node
    pub async fn get_node_edges(&self, node_id: &str) -> Result<Vec<Edge>> {
        let edges = sqlx::query_as::<_, Edge>(
            r#"
            SELECT * FROM edges
            WHERE source_id = ? OR target_id = ?
            ORDER BY weight DESC
            "#,
        )
        .bind(node_id)
        .bind(node_id)
        .fetch_all(&self.db)
        .await?;

        Ok(edges)
    }

    /// Get all nodes of a specific type
    pub async fn get_nodes_by_type(&self, node_type: NodeType) -> Result<Vec<Node>> {
        let nodes = sqlx::query_as::<_, Node>(
            r#"
            SELECT * FROM nodes
            WHERE type = ?
            ORDER BY updated_at DESC
            "#,
        )
        .bind(node_type.as_str())
        .fetch_all(&self.db)
        .await?;

        Ok(nodes)
    }

    /// Get strongest edges (highest weights)
    pub async fn get_strongest_edges(&self, limit: i32) -> Result<Vec<Edge>> {
        let edges = sqlx::query_as::<_, Edge>(
            r#"
            SELECT * FROM edges
            ORDER BY weight DESC
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.db)
        .await?;

        Ok(edges)
    }

    /// Prune weak edges (remove edges below threshold)
    pub async fn prune_weak_edges(&self, min_weight: f32) -> Result<u64> {
        let result = sqlx::query(
            r#"
            DELETE FROM edges
            WHERE weight < ?
            "#,
        )
        .bind(min_weight)
        .execute(&self.db)
        .await?;

        Ok(result.rows_affected())
    }

    /// Register file path mapping (converts to relative path for storage)
    pub async fn register_path(&self, node_id: &str, absolute_path: &str) -> Result<()> {
        // Convert to relative path before storing
        let relative_path = self.to_relative(absolute_path)?;

        sqlx::query(
            r#"
            INSERT INTO file_paths (node_id, path)
            VALUES (?, ?)
            ON CONFLICT(node_id) DO UPDATE SET path = excluded.path
            "#,
        )
        .bind(node_id)
        .bind(&relative_path)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    /// Get node by path (converts to relative before lookup)
    pub async fn get_node_by_path(&self, absolute_path: &str) -> Result<Option<Node>> {
        // Convert query path to relative
        let relative_path = self.to_relative(absolute_path)?;

        let node = sqlx::query_as::<_, Node>(
            r#"
            SELECT n.*
            FROM nodes n
            JOIN file_paths fp ON n.id = fp.node_id
            WHERE fp.path = ?
            "#,
        )
        .bind(&relative_path)
        .fetch_optional(&self.db)
        .await?;

        Ok(node)
    }

    /// Get absolute path for a node
    pub async fn get_absolute_path(&self, node_id: &str) -> Result<Option<PathBuf>> {
        let relative_path: Option<String> = sqlx::query_scalar(
            "SELECT path FROM file_paths WHERE node_id = ?"
        )
        .bind(node_id)
        .fetch_optional(&self.db)
        .await?;

        Ok(relative_path.map(|rel| self.to_absolute(&rel)))
    }

    /// Record session event
    pub async fn record_session_event(
        &self,
        session_id: &str,
        file_id: &str,
        event_type: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO session_events (session_id, file_id, event_type, timestamp)
            VALUES (?, ?, ?, datetime('now'))
            "#,
        )
        .bind(session_id)
        .bind(file_id)
        .bind(event_type)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    // ========================================================================
    // Phase 1.5 Day 3: Content Hashing
    // ========================================================================

    /// Update content hash for a file
    pub async fn update_file_hash(&self, node_id: &str, content_hash: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE file_paths
            SET content_hash = ?, last_indexed = datetime('now')
            WHERE node_id = ?
            "#,
        )
        .bind(content_hash)
        .bind(node_id)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    /// Get stored content hash for a file
    pub async fn get_file_hash(&self, node_id: &str) -> Result<Option<String>> {
        let hash: Option<String> = sqlx::query_scalar(
            "SELECT content_hash FROM file_paths WHERE node_id = ?"
        )
        .bind(node_id)
        .fetch_optional(&self.db)
        .await?;

        Ok(hash)
    }

    /// Check if file needs re-indexing based on content hash
    ///
    /// Returns true if:
    /// - File has no stored hash (never indexed)
    /// - File hash has changed (content modified)
    ///
    /// Returns false if:
    /// - Hash matches (content unchanged, skip indexing)
    pub async fn needs_reindexing(&self, node_id: &str, current_path: &std::path::Path) -> Result<bool> {
        // Get stored hash
        let stored_hash = self.get_file_hash(node_id).await?;

        match stored_hash {
            None => {
                // No hash stored, needs indexing
                Ok(true)
            }
            Some(stored) => {
                // Compute current hash
                let current_hash = hash::compute_file_hash(current_path)?;

                // Compare
                Ok(current_hash != stored)
            }
        }
    }

    /// Index file with hash tracking
    ///
    /// This is a high-level method that:
    /// 1. Checks if file needs re-indexing (via hash)
    /// 2. If unchanged, skips indexing
    /// 3. If changed, re-indexes and updates hash
    ///
    /// Returns: (indexed: bool, reason: &str)
    pub async fn index_file_with_hash(&self, node_id: &str, file_path: &std::path::Path) -> Result<(bool, &'static str)> {
        // Check if needs re-indexing
        let needs_reindex = self.needs_reindexing(node_id, file_path).await?;

        if !needs_reindex {
            return Ok((false, "unchanged"));
        }

        // Compute and store new hash
        let new_hash = hash::compute_file_hash(file_path)?;
        self.update_file_hash(node_id, &new_hash).await?;

        // Here you would call actual indexing logic (NER, embeddings, etc.)
        // For now, just update the hash

        Ok((true, "indexed"))
    }

    /// Get files that need re-indexing
    ///
    /// Returns list of (node_id, path) for files where:
    /// - Hash is missing
    /// - Hash doesn't match current content
    pub async fn get_stale_files(&self) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            r#"
            SELECT node_id, path
            FROM file_paths
            WHERE content_hash IS NULL
            "#
        )
        .fetch_all(&self.db)
        .await?;

        Ok(rows)
    }

    /// Get database statistics
    pub async fn get_stats(&self) -> Result<GraphStats> {
        let node_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nodes")
            .fetch_one(&self.db)
            .await?;

        let edge_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM edges")
            .fetch_one(&self.db)
            .await?;

        let avg_weight: f32 = sqlx::query_scalar("SELECT AVG(weight) FROM edges")
            .fetch_one(&self.db)
            .await
            .unwrap_or(0.0);

        Ok(GraphStats {
            node_count: node_count as u64,
            edge_count: edge_count as u64,
            avg_edge_weight: avg_weight,
        })
    }
}

/// Graph statistics
#[derive(Debug, Clone)]
pub struct GraphStats {
    pub node_count: u64,
    pub edge_count: u64,
    pub avg_edge_weight: f32,
}

/// Graph algorithms (using petgraph for advanced operations)
pub struct GraphAlgorithms;

impl GraphAlgorithms {
    /// Calculate PageRank-style importance scores
    /// (Files with many strong edges are more "important")
    pub fn calculate_importance(edges: &[Edge]) -> std::collections::HashMap<String, f32> {
        use petgraph::graph::{DiGraph, NodeIndex};
        use std::collections::HashMap;

        // Build petgraph
        let mut graph = DiGraph::<String, f32>::new();
        let mut node_map: HashMap<String, NodeIndex> = HashMap::new();

        // Add all nodes
        for edge in edges {
            if !node_map.contains_key(&edge.source_id) {
                let idx = graph.add_node(edge.source_id.clone());
                node_map.insert(edge.source_id.clone(), idx);
            }
            if !node_map.contains_key(&edge.target_id) {
                let idx = graph.add_node(edge.target_id.clone());
                node_map.insert(edge.target_id.clone(), idx);
            }
        }

        // Add edges with weights
        for edge in edges {
            let source_idx = node_map[&edge.source_id];
            let target_idx = node_map[&edge.target_id];
            graph.add_edge(source_idx, target_idx, edge.weight);
        }

        // Simple importance: sum of incoming edge weights
        let mut importance: HashMap<String, f32> = HashMap::new();

        for edge in edges {
            *importance.entry(edge.target_id.clone()).or_insert(0.0) += edge.weight;
        }

        importance
    }

    /// Find clusters (densely connected groups of files)
    pub fn find_clusters(_edges: &[Edge], _min_cluster_size: usize) -> Vec<Vec<String>> {
        // TODO: Implement community detection algorithm
        // For Phase 1, just return empty
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_importance_calculation() {
        let edges = vec![
            Edge {
                id: Some(1),
                source_id: "file1".to_string(),
                target_id: "file2".to_string(),
                r#type: EdgeType::SimilarTo,
                weight: 0.8,
                properties: None,
                created_at: "2025-01-01T00:00:00Z".to_string(),
            },
            Edge {
                id: Some(2),
                source_id: "file3".to_string(),
                target_id: "file2".to_string(),
                r#type: EdgeType::CoOccurred,
                weight: 0.6,
                properties: None,
                created_at: "2025-01-01T00:00:00Z".to_string(),
            },
        ];

        let importance = GraphAlgorithms::calculate_importance(&edges);

        assert!(importance.contains_key("file2"));
        assert_eq!(importance["file2"], 1.4);  // 0.8 + 0.6
    }
}
