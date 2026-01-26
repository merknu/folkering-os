//! Entity extraction pipeline.
//!
//! Orchestrates the full process of extracting entities from files
//! and storing them in the knowledge graph.

use crate::neural::GLiNERService;
use crate::graph::entity_ops;
use crate::models::Node;
use sqlx::SqlitePool;
use anyhow::{Result, Context, bail};
use std::path::Path;
use tokio::fs;

/// Configuration for the entity extraction pipeline
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Entity labels to extract
    pub labels: Vec<String>,
    /// Confidence threshold (0.0 - 1.0)
    pub threshold: f32,
    /// Maximum file size to process (bytes)
    pub max_file_size: usize,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            labels: vec![
                "person".to_string(),
                "project".to_string(),
                "organization".to_string(),
                "location".to_string(),
                "concept".to_string(),
            ],
            threshold: 0.5,
            max_file_size: 10 * 1024 * 1024,  // 10 MB
        }
    }
}

/// Entity extraction pipeline
pub struct EntityPipeline {
    gliner: GLiNERService,
    config: PipelineConfig,
}

impl EntityPipeline {
    /// Create a new entity pipeline
    pub fn new() -> Result<Self> {
        let gliner = GLiNERService::new()
            .context("Failed to create GLiNER service")?;

        Ok(Self {
            gliner,
            config: PipelineConfig::default(),
        })
    }

    /// Create a new entity pipeline with custom configuration
    pub fn with_config(config: PipelineConfig) -> Result<Self> {
        let gliner = GLiNERService::new()
            .context("Failed to create GLiNER service")?;

        Ok(Self { gliner, config })
    }

    /// Process a file and extract entities
    ///
    /// # Arguments
    /// * `db` - Database connection
    /// * `file_path` - Path to file
    /// * `file_node_id` - Node ID of the file in the graph
    ///
    /// # Returns
    /// List of entity nodes created/found
    pub async fn process_file(
        &self,
        db: &SqlitePool,
        file_path: &Path,
        file_node_id: &str,
    ) -> Result<Vec<Node>> {
        // Read file content
        let content = self.read_file_content(file_path).await?;

        // Skip empty files
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Extract entities using GLiNER
        let label_refs: Vec<&str> = self.config.labels.iter().map(|s| s.as_str()).collect();
        let entities = self.gliner.extract_entities(
            &content,
            &label_refs,
            self.config.threshold,
        )?;

        // Process entities (deduplicate and link to file)
        let entity_nodes = entity_ops::process_entities_for_file(
            db,
            file_node_id,
            &entities,
        ).await?;

        Ok(entity_nodes)
    }

    /// Read file content with size limit
    async fn read_file_content(&self, file_path: &Path) -> Result<String> {
        // Check file exists
        if !file_path.exists() {
            bail!("File does not exist: {:?}", file_path);
        }

        // Check file size
        let metadata = fs::metadata(file_path).await
            .context("Failed to get file metadata")?;

        if metadata.len() > self.config.max_file_size as u64 {
            bail!(
                "File too large ({} bytes, max: {} bytes)",
                metadata.len(),
                self.config.max_file_size
            );
        }

        // Read content
        let content = fs::read_to_string(file_path).await
            .context("Failed to read file content")?;

        Ok(content)
    }
}

/// Process a file for entity extraction (convenience function)
///
/// # Arguments
/// * `db` - Database connection
/// * `gliner` - GLiNER service
/// * `file_path` - Path to file
/// * `file_node_id` - Node ID of the file
/// * `labels` - Entity labels to extract
/// * `threshold` - Confidence threshold
///
/// # Returns
/// List of entity nodes
pub async fn process_file_for_entities(
    db: &SqlitePool,
    gliner: &GLiNERService,
    file_path: &Path,
    file_node_id: &str,
    labels: &[&str],
    threshold: f32,
) -> Result<Vec<Node>> {
    // Read file content
    let content = fs::read_to_string(file_path).await
        .context("Failed to read file")?;

    // Skip empty files
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }

    // Extract entities
    let entities = gliner.extract_entities(&content, labels, threshold)?;

    // Process entities
    let entity_nodes = entity_ops::process_entities_for_file(
        db,
        file_node_id,
        &entities,
    ).await?;

    Ok(entity_nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::io::Write;
    use tempfile::TempDir;
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
    #[ignore]  // Requires Python + GLiNER setup
    async fn test_entity_pipeline_basic() {
        let db = setup_test_db().await;
        let temp_dir = TempDir::new().unwrap();

        // Create test file
        let file_path = temp_dir.path().join("test.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(b"Alice and Bob are working on Project Mars").unwrap();

        // Create file node in database
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

        // Create pipeline
        let pipeline = EntityPipeline::new().unwrap();

        // Process file
        let entities = pipeline.process_file(&db, &file_path, &file_id).await.unwrap();

        // Should find Alice, Bob, Project Mars
        assert!(entities.len() >= 2, "Expected at least 2 entities (Alice, Bob)");

        // Verify entities are in database
        let count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM nodes WHERE type IN ('person', 'project')"
        )
        .fetch_one(&db)
        .await
        .unwrap();

        assert!(count.0 >= 2);

        // Verify edges exist
        let edge_count: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM edges WHERE source_id = ? AND type = 'REFERENCES'"
        )
        .bind(&file_id)
        .fetch_one(&db)
        .await
        .unwrap();

        assert!(edge_count.0 >= 2);
    }

    #[tokio::test]
    async fn test_pipeline_empty_file() {
        let db = setup_test_db().await;
        let temp_dir = TempDir::new().unwrap();

        // Create empty file
        let file_path = temp_dir.path().join("empty.txt");
        std::fs::File::create(&file_path).unwrap();

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

        // Create pipeline (will fail if GLiNER not installed, but that's OK for this test)
        if let Ok(pipeline) = EntityPipeline::new() {
            let entities = pipeline.process_file(&db, &file_path, &file_id).await.unwrap();
            assert_eq!(entities.len(), 0);  // Empty file should produce no entities
        }
    }

    #[tokio::test]
    async fn test_pipeline_config() {
        let config = PipelineConfig {
            labels: vec!["person".to_string()],
            threshold: 0.8,
            max_file_size: 1024,
        };

        assert_eq!(config.labels.len(), 1);
        assert_eq!(config.threshold, 0.8);
        assert_eq!(config.max_file_size, 1024);
    }
}
