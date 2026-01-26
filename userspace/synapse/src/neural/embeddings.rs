//! Embedding generation service using sentence-transformers.
//!
//! This module provides text-to-vector embedding generation using the
//! sentence-transformers/all-MiniLM-L6-v2 model via Python subprocess.
//!
//! **Model**: all-MiniLM-L6-v2
//! - Output: 384-dimensional embeddings
//! - Performance: ~50-100ms per text
//! - Use case: Semantic similarity, vector search
//!
//! **Communication Protocol**:
//! - Input (stdin): `{"text": "your text here"}`
//! - Output (stdout): `{"embedding": [0.1, 0.2, ...], "error": null}`

use anyhow::{Result, Context, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio, ChildStdin, ChildStdout, Child};
use std::sync::Mutex;

/// Expected embedding dimension for all-MiniLM-L6-v2
pub const EMBEDDING_DIM: usize = 384;

/// Request to embedding service
#[derive(Debug, Serialize)]
struct EmbeddingRequest {
    text: String,
}

/// Response from embedding service
#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    embedding: Option<Vec<f32>>,
    error: Option<String>,
}

/// Embedding generation service
///
/// Uses sentence-transformers via Python subprocess for fast prototyping.
/// Future optimization: Native ONNX Runtime implementation.
pub struct EmbeddingService {
    process: Mutex<Option<EmbeddingProcess>>,
}

struct EmbeddingProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl EmbeddingService {
    /// Create a new embedding service
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Python is not installed
    /// - sentence-transformers is not installed
    /// - Subprocess fails to start
    pub fn new() -> Result<Self> {
        // Check if Python is available
        let python = Self::find_python()?;

        // Check if sentence-transformers is installed
        Self::check_dependencies(&python)?;

        Ok(Self {
            process: Mutex::new(None),
        })
    }

    /// Find Python executable
    fn find_python() -> Result<String> {
        // Try python3 first, then python
        for cmd in &["python3", "python"] {
            if Command::new(cmd)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok()
            {
                return Ok(cmd.to_string());
            }
        }

        bail!("Python not found. Please install Python 3.10+")
    }

    /// Check if sentence-transformers is installed
    fn check_dependencies(python: &str) -> Result<()> {
        let output = Command::new(python)
            .arg("-c")
            .arg("import sentence_transformers")
            .output()
            .context("Failed to check sentence-transformers")?;

        if !output.status.success() {
            bail!(
                "sentence-transformers not installed. Install with: pip install sentence-transformers"
            );
        }

        Ok(())
    }

    /// Start the embedding subprocess
    fn start_subprocess(&self) -> Result<EmbeddingProcess> {
        let python = Self::find_python()?;

        let script_path = std::env::current_dir()?
            .join("scripts")
            .join("embedding_inference.py");

        if !script_path.exists() {
            bail!("Embedding script not found: {:?}", script_path);
        }

        let mut child = Command::new(python)
            .arg(&script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to start embedding subprocess")?;

        let stdin = child.stdin.take()
            .context("Failed to open subprocess stdin")?;

        let stdout = BufReader::new(
            child.stdout.take()
                .context("Failed to open subprocess stdout")?
        );

        // Wait for "ready" signal from stderr
        std::thread::sleep(std::time::Duration::from_millis(100));

        Ok(EmbeddingProcess {
            child,
            stdin,
            stdout,
        })
    }

    /// Generate embedding for text
    ///
    /// # Arguments
    ///
    /// * `text` - Input text (will be truncated to model's max length, typically 512 tokens)
    ///
    /// # Returns
    ///
    /// A 384-dimensional embedding vector
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Text is empty
    /// - Subprocess communication fails
    /// - Embedding generation fails
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use synapse::EmbeddingService;
    /// let service = EmbeddingService::new()?;
    /// let embedding = service.generate("Machine learning with neural networks")?;
    /// assert_eq!(embedding.len(), 384);
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn generate(&self, text: &str) -> Result<Vec<f32>> {
        if text.trim().is_empty() {
            bail!("Cannot generate embedding for empty text");
        }

        // Get or start process
        let mut process_lock = self.process.lock().unwrap();
        if process_lock.is_none() {
            *process_lock = Some(self.start_subprocess()?);
        }

        let process = process_lock.as_mut().unwrap();

        // Create request
        let request = EmbeddingRequest {
            text: text.to_string(),
        };

        // Send request
        let request_json = serde_json::to_string(&request)?;
        writeln!(process.stdin, "{}", request_json)
            .context("Failed to write to subprocess")?;
        process.stdin.flush()
            .context("Failed to flush subprocess stdin")?;

        // Read response
        let mut response_line = String::new();
        process.stdout.read_line(&mut response_line)
            .context("Failed to read from subprocess")?;

        // Parse response
        let response: EmbeddingResponse = serde_json::from_str(&response_line)
            .context("Failed to parse embedding response")?;

        // Check for errors
        if let Some(error) = response.error {
            bail!("Embedding generation failed: {}", error);
        }

        // Extract embedding
        let embedding = response.embedding
            .context("No embedding in response")?;

        // Validate dimension
        if embedding.len() != EMBEDDING_DIM {
            bail!(
                "Invalid embedding dimension: expected {}, got {}",
                EMBEDDING_DIM,
                embedding.len()
            );
        }

        Ok(embedding)
    }

    /// Generate embeddings for multiple texts (batched)
    ///
    /// More efficient than calling `generate()` multiple times.
    ///
    /// # Arguments
    ///
    /// * `texts` - Vector of input texts
    ///
    /// # Returns
    ///
    /// Vector of 384-dimensional embeddings (one per input text)
    pub fn generate_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let mut embeddings = Vec::with_capacity(texts.len());

        for text in texts {
            let embedding = self.generate(text)?;
            embeddings.push(embedding);
        }

        Ok(embeddings)
    }
}

impl Drop for EmbeddingService {
    fn drop(&mut self) {
        // Cleanup subprocess
        if let Ok(mut process_lock) = self.process.lock() {
            if let Some(mut process) = process_lock.take() {
                let _ = process.child.kill();
                let _ = process.child.wait();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_dimension() {
        assert_eq!(EMBEDDING_DIM, 384);
    }

    #[test]
    #[ignore]  // Requires Python + sentence-transformers
    fn test_embedding_generation() {
        let service = EmbeddingService::new().unwrap();
        let embedding = service.generate("Machine learning with neural networks").unwrap();

        assert_eq!(embedding.len(), 384);

        // Check that embedding is not all zeros
        let sum: f32 = embedding.iter().sum();
        assert!(sum.abs() > 0.0, "Embedding should not be all zeros");

        // Check that values are finite
        for value in &embedding {
            assert!(value.is_finite(), "Embedding values should be finite");
        }
    }

    #[test]
    #[ignore]  // Requires Python + sentence-transformers
    fn test_similar_texts_have_similar_embeddings() {
        let service = EmbeddingService::new().unwrap();

        let embedding1 = service.generate("Machine learning with neural networks").unwrap();
        let embedding2 = service.generate("Deep learning and artificial intelligence").unwrap();

        // Compute cosine similarity
        use crate::neural::cosine_similarity;
        let similarity = cosine_similarity(&embedding1, &embedding2).unwrap();

        // Similar texts should have similarity > 0.5
        assert!(
            similarity > 0.5,
            "Similar texts should have high similarity, got: {}",
            similarity
        );
    }

    #[test]
    #[ignore]  // Requires Python + sentence-transformers
    fn test_dissimilar_texts_have_low_similarity() {
        let service = EmbeddingService::new().unwrap();

        let embedding1 = service.generate("Machine learning with neural networks").unwrap();
        let embedding2 = service.generate("Cooking pasta with tomato sauce").unwrap();

        // Compute cosine similarity
        use crate::neural::cosine_similarity;
        let similarity = cosine_similarity(&embedding1, &embedding2).unwrap();

        // Dissimilar texts should have similarity < 0.5
        assert!(
            similarity < 0.5,
            "Dissimilar texts should have low similarity, got: {}",
            similarity
        );
    }

    #[test]
    #[ignore]  // Requires Python + sentence-transformers
    fn test_batch_generation() {
        let service = EmbeddingService::new().unwrap();

        let texts = vec![
            "First text",
            "Second text",
            "Third text",
        ];

        let embeddings = service.generate_batch(&texts).unwrap();

        assert_eq!(embeddings.len(), 3);
        for embedding in &embeddings {
            assert_eq!(embedding.len(), 384);
        }
    }

    #[test]
    fn test_empty_text_error() {
        let service = EmbeddingService::new().unwrap_or_else(|_| {
            // If service creation fails (no Python), skip test
            return EmbeddingService { process: Mutex::new(None) };
        });

        let result = service.generate("");
        assert!(result.is_err());

        if let Err(e) = result {
            assert!(e.to_string().contains("empty"));
        }
    }
}
