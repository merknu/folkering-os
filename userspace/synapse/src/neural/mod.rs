//! Neural intelligence module for Synapse.
//!
//! This module provides:
//! - Entity extraction using GLiNER
//! - Embedding generation using sentence-transformers
//! - Vector similarity calculations
//!
//! Phase 2 Day 1: GLiNER via Python subprocess (pragmatic approach)
//! Future: Native ONNX inference for better performance

pub mod gliner;
pub mod embeddings;
pub mod similarity;

pub use gliner::{GLiNERService, Entity};
pub use embeddings::{EmbeddingService, EMBEDDING_DIM};
pub use similarity::cosine_similarity;
