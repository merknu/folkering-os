//! Semantic Intent Router (Phase 2)
//!
//! Uses vector embeddings for intelligent intent-to-capability matching.
//! Integrates with Synapse's neural intelligence capabilities.

use crate::types::*;
use std::collections::HashMap;
use serde_json;

/// Semantic router using vector embeddings
pub struct SemanticRouter {
    /// Capability embeddings for similarity matching
    capability_embeddings: HashMap<TaskId, Vec<f32>>,

    /// Capability descriptions (for embedding generation)
    capability_descriptions: HashMap<TaskId, String>,

    /// Embedding service (Python subprocess via Synapse)
    embedding_service: Option<EmbeddingServiceClient>,
}

impl SemanticRouter {
    pub fn new() -> Self {
        // Try to initialize embedding service
        let embedding_service = EmbeddingServiceClient::try_new().ok();

        if embedding_service.is_some() {
            println!("[SEMANTIC] Embedding service available");
        } else {
            println!("[SEMANTIC] Embedding service unavailable (will use pattern matching only)");
        }

        Self {
            capability_embeddings: HashMap::new(),
            capability_descriptions: HashMap::new(),
            embedding_service,
        }
    }

    /// Register a capability with semantic embeddings
    pub fn register_capability(&mut self, capability: &Capability) {
        // Build semantic description of capability
        let description = self.build_capability_description(capability);

        // Generate embedding if service available
        if let Some(service) = &self.embedding_service {
            match service.generate_embedding(&description) {
                Ok(embedding) => {
                    self.capability_embeddings.insert(capability.task_id, embedding);
                    self.capability_descriptions.insert(capability.task_id, description.clone());
                    println!("[SEMANTIC] Generated embedding for {} ({})",
                        capability.task_name, description.len());
                }
                Err(e) => {
                    eprintln!("[SEMANTIC] Failed to generate embedding for {}: {}",
                        capability.task_name, e);
                }
            }
        }
    }

    /// Remove capability embeddings
    pub fn unregister_capability(&mut self, task_id: TaskId) {
        self.capability_embeddings.remove(&task_id);
        self.capability_descriptions.remove(&task_id);
    }

    /// Match intent using semantic similarity
    pub fn match_intent(&self, intent: &Intent, capabilities: &HashMap<TaskId, Capability>) -> Vec<Handler> {
        let embedding_service = match &self.embedding_service {
            Some(service) => service,
            None => return vec![], // No embeddings available
        };

        // Build query from intent
        let query = self.intent_to_query(intent);

        // Generate query embedding
        let query_embedding = match embedding_service.generate_embedding(&query) {
            Ok(emb) => emb,
            Err(e) => {
                eprintln!("[SEMANTIC] Failed to generate query embedding: {}", e);
                return vec![];
            }
        };

        // Compute similarities
        let mut similarities: Vec<(TaskId, f32)> = Vec::new();

        for (task_id, cap_embedding) in &self.capability_embeddings {
            let similarity = cosine_similarity(&query_embedding, cap_embedding);
            similarities.push((*task_id, similarity));
        }

        // Sort by similarity (descending)
        similarities.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        // Convert to handlers
        let mut handlers = Vec::new();
        for (task_id, similarity) in similarities.iter().take(5) {
            if *similarity < 0.3 {
                continue; // Too low similarity
            }

            if let Some(cap) = capabilities.get(task_id) {
                handlers.push(Handler {
                    task_id: *task_id,
                    task_name: cap.task_name.clone(),
                    confidence: *similarity,
                    capabilities: cap.actions.clone(),
                });
            }
        }

        println!("[SEMANTIC] Query: '{}' → {} handlers (top: {:.2})",
            query, handlers.len(),
            handlers.first().map(|h| h.confidence).unwrap_or(0.0));

        handlers
    }

    /// Build semantic description of a capability
    fn build_capability_description(&self, cap: &Capability) -> String {
        // Combine all semantic information into a description
        let mut parts = Vec::new();

        // App name
        parts.push(cap.task_name.clone());

        // Actions
        if !cap.actions.is_empty() {
            parts.push(format!("can {}", cap.actions.join(", ")));
        }

        // File types
        if !cap.file_types.is_empty() {
            parts.push(format!("handles {} files", cap.file_types.join(", ")));
        }

        // Tags
        if !cap.tags.is_empty() {
            parts.push(format!("tags: {}", cap.tags.join(", ")));
        }

        parts.join(". ")
    }

    /// Convert intent to semantic query
    fn intent_to_query(&self, intent: &Intent) -> String {
        match intent {
            Intent::OpenFile { query, .. } => {
                format!("open file {}", query)
            }
            Intent::SendMessage { text, recipients, .. } => {
                format!("send message '{}' to {}", text, recipients.join(", "))
            }
            Intent::RunCommand { command, args, .. } => {
                format!("run command {} {}", command, args.join(" "))
            }
            Intent::Transform { from_format, to_format, operation, .. } => {
                if let Some(op) = operation {
                    format!("{} from {} to {}", op, from_format, to_format)
                } else {
                    format!("convert from {} to {}", from_format, to_format)
                }
            }
            Intent::Create { content_type, initial_content, .. } => {
                if let Some(content) = initial_content {
                    format!("create {} with content '{}'", content_type, content)
                } else {
                    format!("create {}", content_type)
                }
            }
            Intent::Search { query, .. } => {
                format!("search for {}", query)
            }
        }
    }
}

/// Client for embedding generation (wraps Synapse's Python service)
pub struct EmbeddingServiceClient {
    // In a real integration, this would hold connection to Synapse
    // For now, mock implementation
}

impl EmbeddingServiceClient {
    pub fn try_new() -> Result<Self, String> {
        // Check if Python + sentence-transformers available
        match std::process::Command::new("python3")
            .arg("-c")
            .arg("import sentence_transformers")
            .output()
        {
            Ok(output) if output.status.success() => Ok(Self {}),
            _ => Err("sentence-transformers not available".to_string()),
        }
    }

    pub fn generate_embedding(&self, text: &str) -> Result<Vec<f32>, String> {
        // Call Python embedding service (same as Synapse uses)
        let script = r#"
import sys
import json
from sentence_transformers import SentenceTransformer

model = SentenceTransformer('all-MiniLM-L6-v2')
text = sys.stdin.read()
embedding = model.encode(text).tolist()
print(json.dumps(embedding))
"#;

        let mut child = std::process::Command::new("python3")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn python: {}", e))?;

        // Write text to stdin
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())
                .map_err(|e| format!("Failed to write to stdin: {}", e))?;
        }

        // Read output
        let output = child.wait_with_output()
            .map_err(|e| format!("Failed to read output: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Python script failed: {}", stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let embedding: Vec<f32> = serde_json::from_str(&stdout)
            .map_err(|e| format!("Failed to parse embedding: {}", e))?;

        if embedding.len() != 384 {
            return Err(format!("Invalid embedding dimension: {}", embedding.len()));
        }

        Ok(embedding)
    }
}

/// Compute cosine similarity between two vectors
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        return 0.0;
    }

    dot_product / (magnitude_a * magnitude_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 0.001);

        let c = vec![1.0, 0.0, 0.0];
        let d = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&c, &d) - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_capability_description() {
        let router = SemanticRouter::new();

        let cap = Capability {
            task_id: 1,
            task_name: "TextEditor".to_string(),
            actions: vec!["open_file".to_string(), "edit_text".to_string()],
            file_types: vec![".txt".to_string(), ".md".to_string()],
            tags: vec!["editor".to_string(), "productivity".to_string()],
        };

        let desc = router.build_capability_description(&cap);
        assert!(desc.contains("TextEditor"));
        assert!(desc.contains("open_file"));
        assert!(desc.contains("edit_text"));
    }

    #[test]
    fn test_intent_to_query() {
        let router = SemanticRouter::new();

        let intent = Intent::OpenFile {
            query: "my notes.txt".to_string(),
            context: None,
        };

        let query = router.intent_to_query(&intent);
        assert!(query.contains("open"));
        assert!(query.contains("my notes.txt"));
    }
}
