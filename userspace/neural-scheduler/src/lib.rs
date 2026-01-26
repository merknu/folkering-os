//! Neural Scheduler Library
//!
//! Phase 1: Statistical prediction methods
//! Phase 2: ML-based time-series forecasting (Chronos-T5, Mamba)
//!
//! The "Fast Brain" of Folkering OS - makes sub-millisecond scheduling decisions
//! based on predicted resource usage patterns.

pub mod types;
pub mod predictor;
pub mod scheduler;
pub mod bridge_integration;

pub use types::*;
pub use predictor::ResourcePredictor;
pub use scheduler::NeuralScheduler;
pub use bridge_integration::SchedulerBridgeWriter;
