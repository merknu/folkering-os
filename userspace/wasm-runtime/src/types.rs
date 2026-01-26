//! WASM Runtime Types
//!
//! These types map to the WIT interface definitions.

use serde::{Deserialize, Serialize};

/// Intent payload types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Payload {
    Text(String),
    Binary(Vec<u8>),
    FileRef(String),
    Structured(String), // JSON
}

/// Intent metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentMetadata {
    pub source_app: String,
    pub target_app: Option<String>,
    pub timestamp: u64,
    pub priority: u8,
}

/// Intent structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub action: String,
    pub payload: Payload,
    pub metadata: IntentMetadata,
}

/// Routing result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingResult {
    pub matched_apps: Vec<String>,
    pub confidence: f32,
    pub latency_ms: f32,
}

/// Capability definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub action: String,
    pub description: String,
    pub patterns: Vec<String>,
    pub examples: Vec<String>,
}

/// Semantic search result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub file_id: String,
    pub score: f32,
    pub snippet: String,
}

/// App metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMetadata {
    pub app_id: String,
    pub name: String,
    pub version: String,
    pub capabilities: Vec<Capability>,
}

/// WASM module info
#[derive(Debug, Clone)]
pub struct ModuleInfo {
    pub path: String,
    pub app_metadata: AppMetadata,
    pub loaded_at: u64,
}

impl Intent {
    /// Create a new text intent
    pub fn text(action: impl Into<String>, source: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            payload: Payload::Text(text.into()),
            metadata: IntentMetadata {
                source_app: source.into(),
                target_app: None,
                timestamp: current_timestamp(),
                priority: 5,
            },
        }
    }

    /// Create a new binary intent
    pub fn binary(action: impl Into<String>, source: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            action: action.into(),
            payload: Payload::Binary(data),
            metadata: IntentMetadata {
                source_app: source.into(),
                target_app: None,
                timestamp: current_timestamp(),
                priority: 5,
            },
        }
    }

    /// Create a file reference intent
    pub fn file_ref(action: impl Into<String>, source: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            payload: Payload::FileRef(path.into()),
            metadata: IntentMetadata {
                source_app: source.into(),
                target_app: None,
                timestamp: current_timestamp(),
                priority: 5,
            },
        }
    }

    /// Set target app
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.metadata.target_app = Some(target.into());
        self
    }

    /// Set priority
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.metadata.priority = priority;
        self
    }
}

/// Get current timestamp in milliseconds
fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
