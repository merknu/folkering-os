//! Hybrid search using Reciprocal Rank Fusion (RRF).
//!
//! Combines FTS5 keyword search with vector similarity search for better results.
//!
//! **Algorithm**: Reciprocal Rank Fusion (RRF)
//! - Combine rankings from multiple sources
//! - Score = Σ 1/(k + rank_i) for each source
//! - k = 60 (standard RRF constant)
//!
//! **Benefits**:
//! - Better than FTS alone (misses semantic matches)
//! - Better than vector alone (misses exact keyword matches)
//! - Robust to varying score scales

use crate::models::Node;
use crate::query::fts_search::{self, FtsResult};
use crate::graph::vector_ops;
use crate::neural::EmbeddingService;
use sqlx::SqlitePool;
use anyhow::{Result, bail};
use std::collections::HashMap;

/// RRF constant (standard value from research)
const RRF_K: f32 = 60.0;

/// Hybrid search result
#[derive(Debug, Clone)]
pub struct HybridResult {
    pub node: Node,
    pub score: f32,
    pub fts_rank: Option<usize>,    // Position in FTS results (1-indexed)
    pub vector_rank: Option<usize>, // Position in vector results (1-indexed)
}

/// Perform hybrid search using RRF
///
/// # Arguments
///
/// * `db` - Database connection
/// * `embedder` - Embedding service (for query embedding)
/// * `query` - Search query text
/// * `k` - Number of results to return
///
/// # Returns
///
/// List of results ordered by RRF score (best first)
///
/// # Example
///
/// ```no_run
/// # use synapse::{query::hybrid_search, EmbeddingService};
/// let embedder = EmbeddingService::new()?;
/// let results = hybrid_search::search(&db, &embedder, "machine learning", 10).await?;
///
/// for result in results {
///     println!("{}: score={:.4}, fts={:?}, vec={:?}",
///         result.node.id, result.score, result.fts_rank, result.vector_rank);
/// }
/// # Ok::<(), anyhow::Error>(())
/// ```
pub async fn search(
    db: &SqlitePool,
    embedder: &EmbeddingService,
    query: &str,
    k: usize,
) -> Result<Vec<HybridResult>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    // Run both searches in parallel (conceptually)
    // For now, run sequentially

    // 1. FTS search
    let fts_results = fts_search::search(db, query, k * 2).await?;

    // 2. Vector search
    let query_embedding = embedder.generate(query)?;
    let vector_results = vector_ops::search_similar(db, &query_embedding, k * 2).await?;

    // 3. Apply RRF
    let hybrid_results = reciprocal_rank_fusion(&fts_results, &vector_results, k);

    Ok(hybrid_results)
}

/// Reciprocal Rank Fusion algorithm
///
/// Combines rankings from multiple sources into a single ranking.
///
/// Formula: score(d) = Σ 1/(k + rank_i(d))
/// where:
/// - d = document
/// - k = RRF constant (60)
/// - rank_i(d) = rank of d in source i (1-indexed)
///
/// # Arguments
///
/// * `fts_results` - Results from FTS search
/// * `vector_results` - Results from vector search
/// * `k` - Number of top results to return
///
/// # Returns
///
/// Combined results ordered by RRF score
fn reciprocal_rank_fusion(
    fts_results: &[FtsResult],
    vector_results: &[(Node, f32)],
    k: usize,
) -> Vec<HybridResult> {
    let mut scores: HashMap<String, f32> = HashMap::new();
    let mut fts_ranks: HashMap<String, usize> = HashMap::new();
    let mut vector_ranks: HashMap<String, usize> = HashMap::new();
    let mut all_nodes: HashMap<String, Node> = HashMap::new();

    // Process FTS results
    for (rank, result) in fts_results.iter().enumerate() {
        let node_id = result.node.id.clone();
        let rank_1indexed = rank + 1;

        // RRF score contribution from FTS
        let rrf_score = 1.0 / (RRF_K + rank_1indexed as f32);
        *scores.entry(node_id.clone()).or_insert(0.0) += rrf_score;

        fts_ranks.insert(node_id.clone(), rank_1indexed);
        all_nodes.insert(node_id, result.node.clone());
    }

    // Process vector results
    for (rank, (node, _similarity)) in vector_results.iter().enumerate() {
        let node_id = node.id.clone();
        let rank_1indexed = rank + 1;

        // RRF score contribution from vector search
        let rrf_score = 1.0 / (RRF_K + rank_1indexed as f32);
        *scores.entry(node_id.clone()).or_insert(0.0) += rrf_score;

        vector_ranks.insert(node_id.clone(), rank_1indexed);
        all_nodes.insert(node_id, node.clone());
    }

    // Create results
    let mut results: Vec<HybridResult> = scores
        .into_iter()
        .map(|(node_id, score)| {
            let node = all_nodes.get(&node_id).unwrap().clone();
            let fts_rank = fts_ranks.get(&node_id).copied();
            let vector_rank = vector_ranks.get(&node_id).copied();

            HybridResult {
                node,
                score,
                fts_rank,
                vector_rank,
            }
        })
        .collect();

    // Sort by score (descending)
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());

    // Take top k
    results.truncate(k);

    results
}

/// Hybrid search with fallback
///
/// If embedding service unavailable, falls back to FTS-only search.
///
/// # Arguments
///
/// * `db` - Database connection
/// * `embedder` - Optional embedding service
/// * `query` - Search query
/// * `k` - Number of results
///
/// # Returns
///
/// Hybrid results (or FTS-only if no embedder)
pub async fn search_with_fallback(
    db: &SqlitePool,
    embedder: Option<&EmbeddingService>,
    query: &str,
    k: usize,
) -> Result<Vec<HybridResult>> {
    match embedder {
        Some(emb) => search(db, emb, query, k).await,
        None => {
            // FTS-only fallback
            let fts_results = fts_search::search(db, query, k).await?;

            let results = fts_results
                .into_iter()
                .enumerate()
                .map(|(rank, result)| HybridResult {
                    node: result.node,
                    score: result.rank,
                    fts_rank: Some(rank + 1),
                    vector_rank: None,
                })
                .collect();

            Ok(results)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::NodeType;
    use chrono::Utc;

    fn create_test_node(id: &str) -> Node {
        Node {
            id: id.to_string(),
            r#type: NodeType::File,
            properties: "{}".to_string(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn test_rrf_both_sources() {
        let fts_results = vec![
            FtsResult {
                node: create_test_node("doc1"),
                rank: 10.0,
            },
            FtsResult {
                node: create_test_node("doc2"),
                rank: 8.0,
            },
        ];

        let vector_results = vec![
            (create_test_node("doc1"), 0.9),
            (create_test_node("doc3"), 0.7),
        ];

        let results = reciprocal_rank_fusion(&fts_results, &vector_results, 10);

        // doc1 appears in both, should rank highest
        assert_eq!(results[0].node.id, "doc1");
        assert!(results[0].fts_rank.is_some());
        assert!(results[0].vector_rank.is_some());

        // Score should be sum of RRF scores from both sources
        let expected_score = 1.0 / (RRF_K + 1.0) + 1.0 / (RRF_K + 1.0);
        assert!((results[0].score - expected_score).abs() < 0.001);
    }

    #[test]
    fn test_rrf_fts_only() {
        let fts_results = vec![
            FtsResult {
                node: create_test_node("doc1"),
                rank: 10.0,
            },
        ];

        let vector_results = vec![];

        let results = reciprocal_rank_fusion(&fts_results, &vector_results, 10);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node.id, "doc1");
        assert!(results[0].fts_rank.is_some());
        assert!(results[0].vector_rank.is_none());
    }

    #[test]
    fn test_rrf_vector_only() {
        let fts_results = vec![];

        let vector_results = vec![(create_test_node("doc1"), 0.9)];

        let results = reciprocal_rank_fusion(&fts_results, &vector_results, 10);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].node.id, "doc1");
        assert!(results[0].fts_rank.is_none());
        assert!(results[0].vector_rank.is_some());
    }

    #[test]
    fn test_rrf_ranking() {
        let fts_results = vec![
            FtsResult { node: create_test_node("doc1"), rank: 10.0 },
            FtsResult { node: create_test_node("doc2"), rank: 8.0 },
            FtsResult { node: create_test_node("doc3"), rank: 6.0 },
        ];

        let vector_results = vec![
            (create_test_node("doc3"), 0.9),  // doc3 is top in vector
            (create_test_node("doc1"), 0.7),  // doc1 is second
        ];

        let results = reciprocal_rank_fusion(&fts_results, &vector_results, 10);

        // doc1: rank 1 in FTS, rank 2 in vector → high RRF score
        // doc3: rank 3 in FTS, rank 1 in vector → high RRF score
        // doc2: rank 2 in FTS only → lower RRF score

        // Both doc1 and doc3 appear in both sources, should rank above doc2
        let doc1_pos = results.iter().position(|r| r.node.id == "doc1").unwrap();
        let doc3_pos = results.iter().position(|r| r.node.id == "doc3").unwrap();
        let doc2_pos = results.iter().position(|r| r.node.id == "doc2").unwrap();

        assert!(doc1_pos < doc2_pos || doc3_pos < doc2_pos,
            "Documents in both sources should rank higher");
    }

    #[test]
    fn test_rrf_truncation() {
        let fts_results = vec![
            FtsResult { node: create_test_node("doc1"), rank: 10.0 },
            FtsResult { node: create_test_node("doc2"), rank: 8.0 },
            FtsResult { node: create_test_node("doc3"), rank: 6.0 },
        ];

        let vector_results = vec![];

        let results = reciprocal_rank_fusion(&fts_results, &vector_results, 2);

        // Should truncate to top 2
        assert_eq!(results.len(), 2);
    }
}
