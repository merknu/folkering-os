//! Vector similarity calculations.
//!
//! Implements cosine similarity and other vector distance metrics.

use anyhow::{Result, bail};

/// Calculate cosine similarity between two vectors.
///
/// Cosine similarity measures the cosine of the angle between two vectors.
/// Returns a value between -1.0 and 1.0:
/// - 1.0 = identical direction
/// - 0.0 = orthogonal
/// - -1.0 = opposite direction
///
/// # Arguments
/// * `a` - First vector
/// * `b` - Second vector
///
/// # Returns
/// Cosine similarity score
///
/// # Example
/// ```
/// use synapse::neural::cosine_similarity;
///
/// let a = vec![1.0, 0.0, 0.0];
/// let b = vec![1.0, 0.0, 0.0];
/// let similarity = cosine_similarity(&a, &b).unwrap();
/// assert!((similarity - 1.0).abs() < 0.01); // Should be 1.0 (identical)
/// ```
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Result<f32> {
    if a.len() != b.len() {
        bail!("Vectors must have the same length");
    }

    if a.is_empty() {
        bail!("Vectors must not be empty");
    }

    // Calculate dot product
    let dot_product: f32 = a.iter()
        .zip(b.iter())
        .map(|(x, y)| x * y)
        .sum();

    // Calculate magnitudes
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    // Avoid division by zero
    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        return Ok(0.0);
    }

    // Cosine similarity = dot product / (magnitude_a * magnitude_b)
    Ok(dot_product / (magnitude_a * magnitude_b))
}

/// Normalize a vector to unit length (L2 normalization).
///
/// # Arguments
/// * `v` - Vector to normalize
///
/// # Returns
/// Normalized vector
pub fn normalize_vector(v: &[f32]) -> Result<Vec<f32>> {
    if v.is_empty() {
        bail!("Vector must not be empty");
    }

    // Calculate magnitude
    let magnitude: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();

    // Avoid division by zero
    if magnitude == 0.0 {
        return Ok(v.to_vec());
    }

    // Normalize
    Ok(v.iter().map(|x| x / magnitude).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let similarity = cosine_similarity(&a, &b).unwrap();
        assert!((similarity - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let similarity = cosine_similarity(&a, &b).unwrap();
        assert!((similarity - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        let similarity = cosine_similarity(&a, &b).unwrap();
        assert!((similarity - (-1.0)).abs() < 0.01);
    }

    #[test]
    fn test_normalize_vector() {
        let v = vec![3.0, 4.0];
        let normalized = normalize_vector(&v).unwrap();

        // Magnitude should be 1.0
        let magnitude: f32 = normalized.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 0.01);

        // Direction should be preserved (3/5, 4/5)
        assert!((normalized[0] - 0.6).abs() < 0.01);
        assert!((normalized[1] - 0.8).abs() < 0.01);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        assert!(cosine_similarity(&a, &b).is_err());
    }

    #[test]
    fn test_cosine_similarity_empty() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert!(cosine_similarity(&a, &b).is_err());
    }

    #[test]
    fn test_normalize_zero_vector() {
        let v = vec![0.0, 0.0, 0.0];
        let normalized = normalize_vector(&v).unwrap();
        // Zero vector stays zero
        assert_eq!(normalized, vec![0.0, 0.0, 0.0]);
    }
}
