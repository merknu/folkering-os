//! Synapse - Graph Filesystem Library
//!
//! Knowledge graph for files, relationships, and context.

pub mod models;
pub mod observer;
pub mod query;
pub mod graph;
pub mod neural;
pub mod ingestion;

pub use models::{Node, Edge, NodeType, EdgeType};
pub use observer::{Observer, FileAccessSession};
pub use query::{QueryEngine, SessionInfo, SessionEvent, SessionStats};
pub use graph::{GraphDB, GraphStats};
pub use neural::{GLiNERService, Entity, EmbeddingService, EMBEDDING_DIM, cosine_similarity};
pub use ingestion::{
    EntityPipeline, process_file_for_entities, PipelineConfig,
    NeuralPipeline, NeuralConfig, ProcessingResult, process_file_neural,
};
