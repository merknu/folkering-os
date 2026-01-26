//! Content hashing for change detection
//!
//! Uses SHA-256 to compute content hashes for files.
//! This enables skip-on-unchanged optimization: if hash matches,
//! don't re-index the file (even if mtime changed).

use sha2::{Sha256, Digest};
use std::fs::File;
use std::io::{Read, BufReader};
use std::path::Path;
use anyhow::Result;

/// Chunk size for streaming hash computation (8KB)
const CHUNK_SIZE: usize = 8192;

/// Compute SHA-256 hash of file contents
///
/// Streams file in 8KB chunks to handle large files efficiently.
///
/// # Arguments
/// * `path` - Path to file to hash
///
/// # Returns
/// Hex-encoded SHA-256 hash (64 characters)
///
/// # Example
/// ```ignore
/// let hash = compute_file_hash(Path::new("report.pdf"))?;
/// assert_eq!(hash.len(), 64); // SHA-256 = 256 bits = 64 hex chars
/// ```
pub fn compute_file_hash(path: &Path) -> Result<String> {
    // Open file
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);

    // Initialize hasher
    let mut hasher = Sha256::new();

    // Stream file in chunks
    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break; // EOF
        }
        hasher.update(&buffer[..bytes_read]);
    }

    // Finalize and convert to hex string
    let result = hasher.finalize();
    let hash_hex = format!("{:x}", result);

    Ok(hash_hex)
}

/// Fast hash check: returns true if file hash matches expected
///
/// This is optimized for the common case (file unchanged).
/// Computes hash and compares in a single pass.
pub fn hash_matches(path: &Path, expected_hash: &str) -> Result<bool> {
    let actual_hash = compute_file_hash(path)?;
    Ok(actual_hash == expected_hash)
}

/// Compute hash for file content from bytes (for testing)
pub fn compute_bytes_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let result = hasher.finalize();
    format!("{:x}", result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_compute_bytes_hash() {
        let content = b"Hello, world!";
        let hash = compute_bytes_hash(content);

        // SHA-256 of "Hello, world!"
        assert_eq!(hash.len(), 64);

        // Same content = same hash
        let hash2 = compute_bytes_hash(content);
        assert_eq!(hash, hash2);

        // Different content = different hash
        let hash3 = compute_bytes_hash(b"Hello, World!");
        assert_ne!(hash, hash3);
    }

    #[test]
    fn test_compute_file_hash() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("test.txt");

        // Write test content
        let content = b"This is test content for hashing.";
        fs::write(&file_path, content)?;

        // Compute hash
        let hash = compute_file_hash(&file_path)?;

        // Verify hash format
        assert_eq!(hash.len(), 64, "SHA-256 should be 64 hex chars");

        // Verify hash is deterministic
        let hash2 = compute_file_hash(&file_path)?;
        assert_eq!(hash, hash2, "Same file should have same hash");

        Ok(())
    }

    #[test]
    fn test_hash_changes_with_content() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("test.txt");

        // Initial content
        fs::write(&file_path, b"Version 1")?;
        let hash1 = compute_file_hash(&file_path)?;

        // Modified content
        fs::write(&file_path, b"Version 2")?;
        let hash2 = compute_file_hash(&file_path)?;

        // Hashes should differ
        assert_ne!(hash1, hash2, "Different content should have different hash");

        Ok(())
    }

    #[test]
    fn test_hash_matches() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("test.txt");

        // Write content
        fs::write(&file_path, b"Test content")?;
        let hash = compute_file_hash(&file_path)?;

        // Should match
        assert!(hash_matches(&file_path, &hash)?, "Hash should match");

        // Modify file
        fs::write(&file_path, b"Modified content")?;

        // Should not match old hash
        assert!(!hash_matches(&file_path, &hash)?, "Hash should not match after modification");

        Ok(())
    }

    #[test]
    fn test_large_file_streaming() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("large.bin");

        // Create 1MB file
        let mut file = File::create(&file_path)?;
        let chunk = vec![0xAB; 1024]; // 1KB of 0xAB
        for _ in 0..1024 {
            file.write_all(&chunk)?;
        }
        file.sync_all()?;
        drop(file);

        // Should handle large file without loading entire file into memory
        let hash = compute_file_hash(&file_path)?;
        assert_eq!(hash.len(), 64);

        // Verify deterministic
        let hash2 = compute_file_hash(&file_path)?;
        assert_eq!(hash, hash2);

        Ok(())
    }

    #[test]
    fn test_empty_file() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let file_path = temp_dir.path().join("empty.txt");

        // Create empty file
        fs::write(&file_path, b"")?;

        // Should compute hash for empty file
        let hash = compute_file_hash(&file_path)?;
        assert_eq!(hash.len(), 64);

        // SHA-256 of empty string
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(hash, expected);

        Ok(())
    }
}
