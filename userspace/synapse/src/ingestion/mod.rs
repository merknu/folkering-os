//! Ingestion pipeline for processing files and extracting entities.
//!
//! This module orchestrates the full pipeline:
//! 1. Read file content
//! 2. Extract entities using GLiNER
//! 3. Deduplicate entities
//! 4. Store entities in graph
//! 5. Link files to entities
//! 6. Generate embeddings
//! 7. Store embeddings for vector search

pub mod entity_pipeline;
pub mod neural_pipeline;

pub use entity_pipeline::{
    EntityPipeline,
    process_file_for_entities,
    PipelineConfig,
};
pub use neural_pipeline::{
    NeuralPipeline,
    NeuralConfig,
    ProcessingResult,
    process_file_neural,
};
