//! Vector operations for no_std semantic search
//!
//! This module provides:
//! - `Embedding`: Fixed-size 384-dimensional vector (stack-allocated)
//! - `cosine_similarity`: Compute similarity between embeddings
//! - `search_similar`: k-NN search across embeddings table
//!
//! # Memory Budget
//!
//! Each embedding uses 1536 bytes (384 × f32). Search operations use
//! stack-allocated buffers to avoid heap allocation.
//!
//! # Example
//!
//! ```ignore
//! use libsqlite::{SqliteDb, vector::{Embedding, search_similar, SearchResult}};
//!
//! let db = SqliteDb::open(data)?;
//! let query = Embedding::from_blob(query_blob)?;
//!
//! let mut results = [SearchResult::default(); 10];
//! let count = search_similar(&db, &query, 5, &mut results)?;
//!
//! for i in 0..count {
//!     println!("File {}: similarity {:.4}", results[i].file_id, results[i].similarity);
//! }
//! ```

use crate::{Error, SqliteDb, Value};

/// Embedding dimension for all-MiniLM-L6-v2 model
pub const EMBEDDING_DIM: usize = 384;

/// Size of embedding in bytes (384 × f32)
pub const EMBEDDING_SIZE: usize = EMBEDDING_DIM * 4;

/// Fixed-size embedding vector (stack-allocated)
///
/// Uses 1536 bytes on the stack. All values are f32 for efficient
/// similarity computation.
#[derive(Clone, Copy)]
pub struct Embedding {
    /// The 384 float values
    pub values: [f32; EMBEDDING_DIM],
}

impl Default for Embedding {
    fn default() -> Self {
        Self {
            values: [0.0; EMBEDDING_DIM],
        }
    }
}

impl Embedding {
    /// Create a new zero-initialized embedding
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse embedding from SQLite BLOB (little-endian f32 array)
    ///
    /// # Arguments
    ///
    /// * `blob` - Raw bytes from SQLite BLOB column (must be 1536 bytes)
    ///
    /// # Returns
    ///
    /// * `Ok(Embedding)` - Parsed embedding
    /// * `Err(InvalidRecord)` - Blob is wrong size
    ///
    /// # Example
    ///
    /// ```ignore
    /// let blob = record.get(1).and_then(|v| v.as_blob())?;
    /// let embedding = Embedding::from_blob(blob)?;
    /// ```
    pub fn from_blob(blob: &[u8]) -> Result<Self, Error> {
        if blob.len() != EMBEDDING_SIZE {
            return Err(Error::InvalidRecord);
        }

        let mut values = [0.0f32; EMBEDDING_DIM];
        for (i, chunk) in blob.chunks_exact(4).enumerate() {
            values[i] = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }

        Ok(Self { values })
    }

    /// Convert embedding to raw bytes (little-endian f32 array)
    ///
    /// Writes 1536 bytes to the provided buffer.
    ///
    /// # Arguments
    ///
    /// * `buffer` - Output buffer (must be at least 1536 bytes)
    ///
    /// # Returns
    ///
    /// Number of bytes written (always 1536)
    pub fn to_blob(&self, buffer: &mut [u8]) -> usize {
        debug_assert!(buffer.len() >= EMBEDDING_SIZE);
        for (i, &value) in self.values.iter().enumerate() {
            let bytes = value.to_le_bytes();
            buffer[i * 4..(i + 1) * 4].copy_from_slice(&bytes);
        }
        EMBEDDING_SIZE
    }

    /// Compute cosine similarity with another embedding
    ///
    /// Returns a value in range [-1.0, 1.0]:
    /// - 1.0 = identical direction
    /// - 0.0 = orthogonal (unrelated)
    /// - -1.0 = opposite direction
    ///
    /// For normalized embeddings (like sentence-transformers output),
    /// the result is typically in [0.0, 1.0].
    ///
    /// # Arguments
    ///
    /// * `other` - The embedding to compare against
    ///
    /// # Returns
    ///
    /// Cosine similarity score
    ///
    /// # Example
    ///
    /// ```ignore
    /// let similarity = query.cosine_similarity(&document_embedding);
    /// if similarity > 0.7 {
    ///     println!("High similarity!");
    /// }
    /// ```
    pub fn cosine_similarity(&self, other: &Embedding) -> f32 {
        let mut dot = 0.0f32;
        let mut norm_a = 0.0f32;
        let mut norm_b = 0.0f32;

        for i in 0..EMBEDDING_DIM {
            dot += self.values[i] * other.values[i];
            norm_a += self.values[i] * self.values[i];
            norm_b += other.values[i] * other.values[i];
        }

        let norm = sqrt_approx(norm_a * norm_b);
        if norm > 0.0 {
            dot / norm
        } else {
            0.0
        }
    }

    /// Compute L2 (Euclidean) distance to another embedding
    ///
    /// Returns a value >= 0.0. Lower values indicate more similar embeddings.
    ///
    /// # Arguments
    ///
    /// * `other` - The embedding to compare against
    ///
    /// # Returns
    ///
    /// L2 distance
    pub fn l2_distance(&self, other: &Embedding) -> f32 {
        let mut sum = 0.0f32;
        for i in 0..EMBEDDING_DIM {
            let diff = self.values[i] - other.values[i];
            sum += diff * diff;
        }
        sqrt_approx(sum)
    }

    /// Compute dot product with another embedding
    ///
    /// For normalized embeddings, this is equivalent to cosine similarity.
    ///
    /// # Arguments
    ///
    /// * `other` - The embedding to compute dot product with
    ///
    /// # Returns
    ///
    /// Dot product value
    pub fn dot_product(&self, other: &Embedding) -> f32 {
        let mut sum = 0.0f32;
        for i in 0..EMBEDDING_DIM {
            sum += self.values[i] * other.values[i];
        }
        sum
    }

    /// Check if embedding is all zeros
    pub fn is_zero(&self) -> bool {
        self.values.iter().all(|&v| v == 0.0)
    }

    /// Compute the L2 norm (magnitude) of the embedding
    pub fn norm(&self) -> f32 {
        let sum: f32 = self.values.iter().map(|&v| v * v).sum();
        sqrt_approx(sum)
    }
}

/// Fast approximate square root for no_std (Newton-Raphson method)
///
/// Accurate to about 4 decimal places for typical embedding values.
fn sqrt_approx(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }

    // Initial guess using bit manipulation (Quake III fast inverse sqrt style)
    let mut guess = x;
    let x_half = 0.5 * x;
    let mut i = guess.to_bits();
    i = 0x5f375a86 - (i >> 1); // Magic constant for sqrt
    guess = f32::from_bits(i);

    // Newton-Raphson iterations for 1/sqrt(x)
    guess = guess * (1.5 - x_half * guess * guess);
    guess = guess * (1.5 - x_half * guess * guess);

    // Return sqrt(x) = x * (1/sqrt(x))
    x * guess
}

/// Vector search result
#[derive(Clone, Copy, Debug)]
pub struct SearchResult {
    /// File ID from the embeddings table
    pub file_id: u32,
    /// Cosine similarity score (higher = more similar)
    pub similarity: f32,
}

impl Default for SearchResult {
    fn default() -> Self {
        Self {
            file_id: 0,
            similarity: -1.0, // Use -1 so empty slots sort to end
        }
    }
}

impl SearchResult {
    /// Create a new search result
    pub fn new(file_id: u32, similarity: f32) -> Self {
        Self { file_id, similarity }
    }
}

/// Search for similar files using brute-force k-NN
///
/// Scans the `embeddings` table and returns the top `k` most similar
/// files based on cosine similarity.
///
/// # Arguments
///
/// * `db` - Open SQLite database
/// * `query` - Query embedding to search for
/// * `k` - Maximum number of results to return
/// * `results` - Output buffer for results (must have at least `k` slots)
///
/// # Returns
///
/// * `Ok(count)` - Number of results found (0 to k)
/// * `Err(...)` - Database error
///
/// # Example
///
/// ```ignore
/// let mut results = [SearchResult::default(); 10];
/// let count = search_similar(&db, &query, 5, &mut results)?;
/// for i in 0..count {
///     println!("Match: file_id={}, similarity={:.4}",
///         results[i].file_id, results[i].similarity);
/// }
/// ```
///
/// # Performance
///
/// This is O(n) where n is the number of embeddings. For small datasets
/// (< 1000 files), this is fast enough for interactive use (~2ms for 100 files).
/// For larger datasets, consider HNSW indexing.
pub fn search_similar<'a>(
    db: &SqliteDb<'a>,
    query: &Embedding,
    k: usize,
    results: &mut [SearchResult],
) -> Result<usize, Error> {
    if k == 0 || results.is_empty() {
        return Ok(0);
    }

    let max_results = k.min(results.len());

    // Initialize results with default values
    for r in results.iter_mut().take(max_results) {
        *r = SearchResult::default();
    }

    let scanner = db.table_scan("embeddings")?;
    let mut count = 0;

    for record_result in scanner {
        let record = record_result?;

        // embeddings table: file_id (col 0), vector (col 1), model (col 2), created_at (col 3)
        let file_id = match record.get(0) {
            Some(Value::Integer(id)) => id as u32,
            _ => continue,
        };

        let embedding = match record.get(1) {
            Some(Value::Blob(blob)) => match Embedding::from_blob(blob) {
                Ok(e) => e,
                Err(_) => continue,
            },
            _ => continue,
        };

        let similarity = query.cosine_similarity(&embedding);

        // Insert into results if better than worst result
        if count < max_results || similarity > results[max_results - 1].similarity {
            let result = SearchResult::new(file_id, similarity);
            insert_sorted(results, &mut count, max_results, result);
        }
    }

    Ok(count)
}

/// Insert a result into a sorted array (descending by similarity)
fn insert_sorted(
    results: &mut [SearchResult],
    count: &mut usize,
    max_results: usize,
    result: SearchResult,
) {
    // Find insertion point (binary search would be faster but this is simpler)
    let mut pos = *count;
    while pos > 0 && results[pos - 1].similarity < result.similarity {
        pos -= 1;
    }

    // Shift elements down
    if pos < max_results {
        let shift_end = (*count).min(max_results - 1);
        for i in (pos..shift_end).rev() {
            results[i + 1] = results[i];
        }
        results[pos] = result;
        if *count < max_results {
            *count += 1;
        }
    }
}

/// Get embedding for a specific file ID
///
/// # Arguments
///
/// * `db` - Open SQLite database
/// * `file_id` - File ID to look up
///
/// # Returns
///
/// * `Ok(Some(embedding))` - Found embedding
/// * `Ok(None)` - File ID not found
/// * `Err(...)` - Database error
pub fn get_embedding_by_file_id<'a>(
    db: &SqliteDb<'a>,
    file_id: u32,
) -> Result<Option<Embedding>, Error> {
    let scanner = db.table_scan("embeddings")?;

    for record_result in scanner {
        let record = record_result?;

        // Check file_id (column 0)
        if let Some(Value::Integer(id)) = record.get(0) {
            if id as u32 == file_id {
                // Get vector blob (column 1)
                if let Some(Value::Blob(blob)) = record.get(1) {
                    return Embedding::from_blob(blob).map(Some);
                }
            }
        }
    }

    Ok(None)
}

/// Count total embeddings in database
pub fn count_embeddings<'a>(db: &SqliteDb<'a>) -> Result<usize, Error> {
    let scanner = db.table_scan("embeddings")?;
    let mut count = 0;
    for _ in scanner {
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedding_default() {
        let e = Embedding::default();
        assert!(e.is_zero());
        assert_eq!(e.norm(), 0.0);
    }

    #[test]
    fn test_embedding_from_blob() {
        // Create a simple embedding with known values
        let mut blob = [0u8; EMBEDDING_SIZE];
        for i in 0..EMBEDDING_DIM {
            let val = (i as f32) / 100.0;
            let bytes = val.to_le_bytes();
            blob[i * 4..(i + 1) * 4].copy_from_slice(&bytes);
        }

        let embedding = Embedding::from_blob(&blob).unwrap();
        assert!((embedding.values[0] - 0.0).abs() < 0.0001);
        assert!((embedding.values[100] - 1.0).abs() < 0.0001);
    }

    #[test]
    fn test_embedding_from_blob_wrong_size() {
        let blob = [0u8; 100]; // Wrong size
        assert!(Embedding::from_blob(&blob).is_err());
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let mut e = Embedding::default();
        for i in 0..EMBEDDING_DIM {
            e.values[i] = (i as f32) / 100.0;
        }

        let similarity = e.cosine_similarity(&e);
        assert!((similarity - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let mut e1 = Embedding::default();
        let mut e2 = Embedding::default();

        // Create two orthogonal vectors
        e1.values[0] = 1.0;
        e2.values[1] = 1.0;

        let similarity = e1.cosine_similarity(&e2);
        assert!(similarity.abs() < 0.001);
    }

    #[test]
    fn test_sqrt_approx() {
        assert!((sqrt_approx(4.0) - 2.0).abs() < 0.01);
        assert!((sqrt_approx(9.0) - 3.0).abs() < 0.01);
        assert!((sqrt_approx(2.0) - 1.414).abs() < 0.01);
        assert_eq!(sqrt_approx(0.0), 0.0);
    }

    #[test]
    fn test_search_result_default() {
        let r = SearchResult::default();
        assert_eq!(r.file_id, 0);
        assert!(r.similarity < 0.0);
    }

    #[test]
    fn test_insert_sorted() {
        let mut results = [SearchResult::default(); 5];
        let mut count = 0;

        insert_sorted(&mut results, &mut count, 5, SearchResult::new(1, 0.5));
        assert_eq!(count, 1);
        assert_eq!(results[0].file_id, 1);

        insert_sorted(&mut results, &mut count, 5, SearchResult::new(2, 0.8));
        assert_eq!(count, 2);
        assert_eq!(results[0].file_id, 2); // Higher similarity first
        assert_eq!(results[1].file_id, 1);

        insert_sorted(&mut results, &mut count, 5, SearchResult::new(3, 0.6));
        assert_eq!(count, 3);
        assert_eq!(results[0].file_id, 2); // 0.8
        assert_eq!(results[1].file_id, 3); // 0.6
        assert_eq!(results[2].file_id, 1); // 0.5
    }

    #[test]
    fn test_to_blob() {
        let mut e = Embedding::default();
        e.values[0] = 1.5;
        e.values[383] = -2.5;

        let mut buffer = [0u8; EMBEDDING_SIZE];
        let len = e.to_blob(&mut buffer);

        assert_eq!(len, EMBEDDING_SIZE);

        // Parse back and verify
        let e2 = Embedding::from_blob(&buffer).unwrap();
        assert!((e2.values[0] - 1.5).abs() < 0.0001);
        assert!((e2.values[383] - (-2.5)).abs() < 0.0001);
    }
}
