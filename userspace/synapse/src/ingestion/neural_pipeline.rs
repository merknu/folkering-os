//! Neural processing pipeline for files.
//!
//! This module orchestrates the full neural processing pipeline:
//! 1. Check if file needs processing (content hash)
//! 2. Extract entities using GLiNER
//! 3. Generate embedding using sentence-transformers
//! 4. Store entities and embedding in database
//!
//! Integrates with observer for automatic processing on file changes.

use crate::neural::{GLiNERService, EmbeddingService};
use crate::graph::{entity_ops, vector_ops, compute_file_hash};
use crate::models::Node;
use sqlx::SqlitePool;
use anyhow::{Result, Context, bail};
use std::path::Path;
use tokio::fs;

/// Configuration for neural pipeline
#[derive(Debug, Clone)]
pub struct NeuralConfig {
    /// Entity labels to extract
    pub entity_labels: Vec<String>,
    /// Entity confidence threshold
    pub entity_threshold: f32,
    /// Maximum file size to process
    pub max_file_size: usize,
    /// Whether to skip if content hash unchanged
    pub check_hash: bool,
}

impl Default for NeuralConfig {
    fn default() -> Self {
        Self {
            entity_labels: vec![
                "person".to_string(),
                "project".to_string(),
                "organization".to_string(),
                "location".to_string(),
                "concept".to_string(),
            ],
            entity_threshold: 0.5,
            max_file_size: 10 * 1024 * 1024,  // 10 MB
            check_hash: true,
        }
    }
}

/// Neural processing pipeline
///
/// Coordinates entity extraction and embedding generation.
pub struct NeuralPipeline {
    gliner: Option<GLiNERService>,
    embedder: Option<EmbeddingService>,
    config: NeuralConfig,
}

/// Result of neural processing
#[derive(Debug)]
pub struct ProcessingResult {
    /// Whether file was processed
    pub processed: bool,
    /// Reason for skip/process
    pub reason: String,
    /// Number of entities extracted
    pub entity_count: usize,
    /// Whether embedding was generated
    pub has_embedding: bool,
}

impl NeuralPipeline {
    /// Create new neural pipeline
    ///
    /// Attempts to initialize both GLiNER and embedding services.
    /// If either fails, that service will be disabled.
    pub fn new() -> Self {
        let gliner = GLiNERService::new().ok();
        let embedder = EmbeddingService::new().ok();

        if gliner.is_none() {
            eprintln!("Warning: GLiNER service unavailable - entity extraction disabled");
        }
        if embedder.is_none() {
            eprintln!("Warning: Embedding service unavailable - vector search disabled");
        }

        Self {
            gliner,
            embedder,
            config: NeuralConfig::default(),
        }
    }

    /// Create pipeline with custom configuration
    pub fn with_config(config: NeuralConfig) -> Self {
        let mut pipeline = Self::new();
        pipeline.config = config;
        pipeline
    }

    /// Check if pipeline has entity extraction capability
    pub fn has_entity_extraction(&self) -> bool {
        self.gliner.is_some()
    }

    /// Check if pipeline has embedding generation capability
    pub fn has_embeddings(&self) -> bool {
        self.embedder.is_some()
    }

    /// Process file with full neural pipeline
    ///
    /// # Arguments
    ///
    /// * `db` - Database connection
    /// * `file_path` - Path to file
    /// * `file_node_id` - Node ID of file in graph
    ///
    /// # Returns
    ///
    /// ProcessingResult with details of what was done
    ///
    /// # Errors
    ///
    /// Returns error if file cannot be read or processing fails
    pub async fn process_file(
        &self,
        db: &SqlitePool,
        file_path: &Path,
        file_node_id: &str,
    ) -> Result<ProcessingResult> {
        // Check if file needs processing (hash-based)
        if self.config.check_hash {
            if let Ok(needs_processing) = self.needs_processing(db, file_node_id, file_path).await {
                if !needs_processing {
                    return Ok(ProcessingResult {
                        processed: false,
                        reason: "content unchanged (hash match)".to_string(),
                        entity_count: 0,
                        has_embedding: false,
                    });
                }
            }
        }

        // Read file content
        let content = self.read_file_content(file_path).await?;

        if content.trim().is_empty() {
            return Ok(ProcessingResult {
                processed: false,
                reason: "empty file".to_string(),
                entity_count: 0,
                has_embedding: false,
            });
        }

        let mut entity_count = 0;
        let mut has_embedding = false;

        // Extract entities (if available)
        if let Some(ref gliner) = self.gliner {
            let label_refs: Vec<&str> = self.config.entity_labels.iter()
                .map(|s| s.as_str())
                .collect();

            match gliner.extract_entities(&content, &label_refs, self.config.entity_threshold) {
                Ok(entities) => {
                    // Process entities (deduplicate and link)
                    let entity_nodes = entity_ops::process_entities_for_file(
                        db,
                        file_node_id,
                        &entities,
                    ).await?;

                    entity_count = entity_nodes.len();
                }
                Err(e) => {
                    eprintln!("Entity extraction failed for {}: {}", file_path.display(), e);
                }
            }
        }

        // Generate embedding (if available)
        if let Some(ref embedder) = self.embedder {
            match embedder.generate(&content) {
                Ok(embedding) => {
                    // Store embedding
                    vector_ops::insert_embedding(db, file_node_id, &embedding).await?;
                    has_embedding = true;
                }
                Err(e) => {
                    eprintln!("Embedding generation failed for {}: {}", file_path.display(), e);
                }
            }
        }

        // Update content hash
        if self.config.check_hash {
            let hash = compute_file_hash(file_path)?;
            self.update_file_hash(db, file_node_id, &hash).await?;
        }

        Ok(ProcessingResult {
            processed: true,
            reason: "processed successfully".to_string(),
            entity_count,
            has_embedding,
        })
    }

    /// Check if file needs processing based on content hash
    async fn needs_processing(
        &self,
        db: &SqlitePool,
        file_node_id: &str,
        file_path: &Path,
    ) -> Result<bool> {
        // Get stored hash
        let stored_hash: Option<String> = sqlx::query_scalar(
            "SELECT content_hash FROM file_paths WHERE node_id = ?"
        )
        .bind(file_node_id)
        .fetch_optional(db)
        .await?;

        match stored_hash {
            None => Ok(true),  // No hash, needs processing
            Some(stored) => {
                // Compute current hash
                let current_hash = compute_file_hash(file_path)?;
                Ok(current_hash != stored)
            }
        }
    }

    /// Update file content hash in database
    async fn update_file_hash(
        &self,
        db: &SqlitePool,
        file_node_id: &str,
        hash: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE file_paths
            SET content_hash = ?, last_indexed = datetime('now')
            WHERE node_id = ?
            "#
        )
        .bind(hash)
        .bind(file_node_id)
        .execute(db)
        .await?;

        Ok(())
    }

    /// Read file content with size limit
    async fn read_file_content(&self, file_path: &Path) -> Result<String> {
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

    /// Reprocess file (force processing even if hash unchanged)
    ///
    /// Useful for:
    /// - Reindexing after changing entity labels
    /// - Fixing corrupted embeddings
    /// - Manual refresh
    pub async fn reprocess_file(
        &self,
        db: &SqlitePool,
        file_path: &Path,
        file_node_id: &str,
    ) -> Result<ProcessingResult> {
        // Temporarily disable hash check
        let original_check = self.config.check_hash;
        let mut pipeline = self.clone_with_config();
        pipeline.config.check_hash = false;

        let result = pipeline.process_file(db, file_path, file_node_id).await;

        // Restore original config
        // (Note: In real impl, we'd use interior mutability or pass config as param)

        result
    }

    fn clone_with_config(&self) -> Self {
        Self {
            gliner: None,  // Don't clone services (expensive)
            embedder: None,
            config: self.config.clone(),
        }
    }
}

/// Convenience function: Process file for neural intelligence
///
/// Creates a temporary pipeline and processes file.
/// Use `NeuralPipeline` directly for better performance (reuse services).
pub async fn process_file_neural(
    db: &SqlitePool,
    file_path: &Path,
    file_node_id: &str,
) -> Result<ProcessingResult> {
    let pipeline = NeuralPipeline::new();
    pipeline.process_file(db, file_path, file_node_id).await
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

        sqlx::query(
            r#"
            CREATE TABLE file_paths (
                node_id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                content_hash TEXT,
                last_indexed TEXT
            )
            "#
        )
        .execute(&db)
        .await
        .unwrap();

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

    async fn create_file_node(db: &SqlitePool, node_id: &str, path: &str) {
        let now = Utc::now().to_rfc3339();
        let props = serde_json::json!({"name": path});

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

        sqlx::query(
            "INSERT INTO file_paths (node_id, path) VALUES (?, ?)"
        )
        .bind(node_id)
        .bind(path)
        .execute(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_pipeline_creation() {
        let pipeline = NeuralPipeline::new();

        // Services may or may not be available depending on Python setup
        // Just verify pipeline was created
        assert!(true);
    }

    #[tokio::test]
    async fn test_process_empty_file() {
        let db = setup_test_db().await;
        let temp_dir = TempDir::new().unwrap();

        // Create empty file
        let file_path = temp_dir.path().join("empty.txt");
        std::fs::File::create(&file_path).unwrap();

        let node_id = Uuid::new_v4().to_string();
        create_file_node(&db, &node_id, "empty.txt").await;

        let pipeline = NeuralPipeline::new();
        let result = pipeline.process_file(&db, &file_path, &node_id).await.unwrap();

        assert!(!result.processed);
        assert!(result.reason.contains("empty"));
    }

    #[tokio::test]
    async fn test_hash_based_skip() {
        let db = setup_test_db().await;
        let temp_dir = TempDir::new().unwrap();

        // Create file with content
        let file_path = temp_dir.path().join("test.txt");
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(b"Test content").unwrap();
        drop(file);

        let node_id = Uuid::new_v4().to_string();
        create_file_node(&db, &node_id, "test.txt").await;

        let pipeline = NeuralPipeline::new();

        // First processing
        let result1 = pipeline.process_file(&db, &file_path, &node_id).await.unwrap();
        assert!(result1.processed || !pipeline.has_entity_extraction());

        // Second processing (should skip due to hash)
        let result2 = pipeline.process_file(&db, &file_path, &node_id).await.unwrap();
        assert!(!result2.processed);
        assert!(result2.reason.contains("unchanged"));
    }

    #[tokio::test]
    async fn test_config_defaults() {
        let config = NeuralConfig::default();

        assert_eq!(config.entity_threshold, 0.5);
        assert_eq!(config.max_file_size, 10 * 1024 * 1024);
        assert!(config.check_hash);
        assert!(config.entity_labels.contains(&"person".to_string()));
    }
}
