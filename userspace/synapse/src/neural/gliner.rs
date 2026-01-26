//! GLiNER entity extraction service.
//!
//! Phase 2 Day 1 Implementation: Python subprocess
//!
//! This implementation calls GLiNER via Python subprocess for rapid prototyping.
//! The communication protocol uses JSON for simplicity.
//!
//! Future improvements:
//! - Native ONNX inference using `ort` crate (Day 2-3)
//! - Batch processing for efficiency
//! - GPU acceleration via CUDA

use anyhow::{Result, Context, bail};
use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};
use std::io::Write;

/// GLiNER entity extraction service
pub struct GLiNERService {
    python_path: String,
    script_path: String,
}

/// Extracted entity with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    /// Entity text (e.g., "Alice")
    pub text: String,
    /// Entity label (e.g., "person", "project", "concept")
    pub label: String,
    /// Confidence score (0.0 - 1.0)
    pub confidence: f32,
    /// Start character offset in source text
    pub start: usize,
    /// End character offset in source text
    pub end: usize,
}

/// Request to GLiNER Python subprocess
#[derive(Debug, Serialize)]
struct GLiNERRequest {
    text: String,
    labels: Vec<String>,
    threshold: f32,
}

/// Response from GLiNER Python subprocess
#[derive(Debug, Deserialize)]
struct GLiNERResponse {
    entities: Vec<Entity>,
    #[serde(default)]
    error: Option<String>,
}

impl GLiNERService {
    /// Create a new GLiNER service
    ///
    /// This will use Python subprocess for inference.
    /// Requires Python 3.10+ with `gliner` installed.
    pub fn new() -> Result<Self> {
        // Find Python executable
        let python_path = Self::find_python()?;

        // Find GLiNER inference script
        let script_path = Self::find_inference_script()?;

        Ok(Self {
            python_path,
            script_path,
        })
    }

    /// Extract entities from text
    ///
    /// # Arguments
    /// * `text` - Text to analyze
    /// * `labels` - Entity types to extract (e.g., ["person", "project", "concept"])
    /// * `threshold` - Confidence threshold (0.0 - 1.0)
    ///
    /// # Example
    /// ```no_run
    /// use synapse::neural::GLiNERService;
    ///
    /// let gliner = GLiNERService::new()?;
    /// let entities = gliner.extract_entities(
    ///     "Alice and Bob discussed physics",
    ///     &["person", "concept"],
    ///     0.5
    /// )?;
    ///
    /// for entity in entities {
    ///     println!("{} ({}): {:.2}", entity.text, entity.label, entity.confidence);
    /// }
    /// ```
    pub fn extract_entities(
        &self,
        text: &str,
        labels: &[&str],
        threshold: f32,
    ) -> Result<Vec<Entity>> {
        // Validate inputs
        if text.is_empty() {
            return Ok(Vec::new());
        }

        if labels.is_empty() {
            bail!("At least one label must be provided");
        }

        if !(0.0..=1.0).contains(&threshold) {
            bail!("Threshold must be between 0.0 and 1.0");
        }

        // Build request
        let request = GLiNERRequest {
            text: text.to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            threshold,
        };

        let request_json = serde_json::to_string(&request)
            .context("Failed to serialize GLiNER request")?;

        // Spawn Python subprocess
        let mut child = Command::new(&self.python_path)
            .arg(&self.script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn Python subprocess")?;

        // Send request via stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(request_json.as_bytes())
                .context("Failed to write to subprocess stdin")?;
        }

        // Wait for completion
        let output = child.wait_with_output()
            .context("Failed to wait for subprocess")?;

        // Check exit status
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("GLiNER subprocess failed: {}", stderr);
        }

        // Parse response
        let response: GLiNERResponse = serde_json::from_slice(&output.stdout)
            .context("Failed to parse GLiNER response")?;

        // Check for errors
        if let Some(error) = response.error {
            bail!("GLiNER error: {}", error);
        }

        Ok(response.entities)
    }

    /// Find Python executable
    fn find_python() -> Result<String> {
        // Try common Python executables
        for python in &["python3", "python", "py"] {
            if Command::new(python)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return Ok(python.to_string());
            }
        }

        bail!("Python not found. Please install Python 3.10+");
    }

    /// Find GLiNER inference script
    fn find_inference_script() -> Result<String> {
        // Try to find the script relative to the current binary
        let candidates = vec![
            "scripts/gliner_inference.py",
            "../scripts/gliner_inference.py",
            "../../scripts/gliner_inference.py",
        ];

        for path in candidates {
            if std::path::Path::new(path).exists() {
                return Ok(path.to_string());
            }
        }

        bail!("GLiNER inference script not found. Please run from project root.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entity_structure() {
        let entity = Entity {
            text: "Alice".to_string(),
            label: "person".to_string(),
            confidence: 0.95,
            start: 0,
            end: 5,
        };

        assert_eq!(entity.text, "Alice");
        assert_eq!(entity.label, "person");
        assert!((entity.confidence - 0.95).abs() < 0.01);
    }

    #[test]
    #[ignore] // Requires Python + GLiNER installed
    fn test_gliner_extraction() {
        let gliner = GLiNERService::new().expect("Failed to create GLiNER service");

        let entities = gliner.extract_entities(
            "Alice and Bob discussed physics",
            &["person", "concept"],
            0.5
        ).expect("Failed to extract entities");

        // Should find at least Alice and Bob
        assert!(entities.len() >= 2);

        // Check that we found people
        let people: Vec<_> = entities.iter()
            .filter(|e| e.label == "person")
            .collect();
        assert!(people.len() >= 2);
    }
}
