//! Shadow table chunk readers for quantized vector search
//!
//! Shadow tables store pre-quantized vectors in chunks for efficient batch processing:
//! - `shadow_bq`: Binary quantized vectors (64 vectors per 3072-byte chunk)
//! - `shadow_sq8`: Scalar quantized vectors (8 vectors per ~3200-byte chunk)
//!
//! The chunk layout is designed to fit within a single 4KB page while maximizing
//! batch size for SIMD operations.

use crate::quantize::{BinaryVector, ScalarVector, BQ_SIZE, SQ8_SERIALIZED_SIZE};
use crate::{Error, SqliteDb, Value};

/// Number of BQ vectors per chunk (64 × 48 = 3072 bytes)
pub const BQ_CHUNK_SIZE: usize = 64;

/// Number of SQ8 vectors per chunk (8 × 392 = 3136 bytes)
pub const SQ8_CHUNK_SIZE: usize = 8;

/// Raw size of a BQ chunk in bytes
pub const BQ_CHUNK_BYTES: usize = BQ_CHUNK_SIZE * BQ_SIZE;

/// Raw size of an SQ8 chunk in bytes
pub const SQ8_CHUNK_BYTES: usize = SQ8_CHUNK_SIZE * SQ8_SERIALIZED_SIZE;

/// Metadata about a vector in shadow tables
#[derive(Clone, Copy, Debug, Default)]
pub struct VectorMeta {
    /// Original rowid in the embeddings table
    pub user_rowid: u32,
    /// Chunk ID in shadow_bq table
    pub bq_chunk_id: u32,
    /// Offset within the BQ chunk (0-63)
    pub bq_offset: u8,
    /// Chunk ID in shadow_sq8 table
    pub sq8_chunk_id: u32,
    /// Offset within the SQ8 chunk (0-7)
    pub sq8_offset: u8,
}

/// Reader for BQ shadow table chunks
pub struct BQChunkReader<'a> {
    db: &'a SqliteDb<'a>,
    current_chunk_id: u32,
    current_data: Option<&'a [u8]>,
}

impl<'a> BQChunkReader<'a> {
    /// Create a new BQ chunk reader
    pub fn new(db: &'a SqliteDb<'a>) -> Self {
        Self {
            db,
            current_chunk_id: u32::MAX,
            current_data: None,
        }
    }

    /// Load a chunk by ID
    fn load_chunk(&mut self, chunk_id: u32) -> Result<(), Error> {
        if self.current_chunk_id == chunk_id && self.current_data.is_some() {
            return Ok(());
        }

        let scanner = self.db.table_scan("shadow_bq")?;
        for result in scanner {
            let record = result?;
            if let Some(Value::Integer(id)) = record.get(0) {
                if id as u32 == chunk_id {
                    if let Some(Value::Blob(data)) = record.get(1) {
                        if data.len() >= BQ_CHUNK_BYTES {
                            self.current_chunk_id = chunk_id;
                            self.current_data = Some(data);
                            return Ok(());
                        }
                    }
                }
            }
        }

        Err(Error::TableNotFound)
    }

    /// Get a BQ vector from a specific chunk and offset
    pub fn get_vector(&mut self, chunk_id: u32, offset: u8) -> Result<BinaryVector, Error> {
        if offset as usize >= BQ_CHUNK_SIZE {
            return Err(Error::InvalidRecord);
        }

        self.load_chunk(chunk_id)?;

        let data = self.current_data.ok_or(Error::InvalidRecord)?;
        let start = (offset as usize) * BQ_SIZE;
        let end = start + BQ_SIZE;

        if end > data.len() {
            return Err(Error::InvalidRecord);
        }

        BinaryVector::from_bytes(&data[start..end]).ok_or(Error::InvalidRecord)
    }

    /// Get all vectors from a chunk
    pub fn get_chunk_vectors(&mut self, chunk_id: u32, count: usize) -> Result<[BinaryVector; BQ_CHUNK_SIZE], Error> {
        self.load_chunk(chunk_id)?;

        let data = self.current_data.ok_or(Error::InvalidRecord)?;
        let mut vectors = [BinaryVector::default(); BQ_CHUNK_SIZE];

        let actual_count = count.min(BQ_CHUNK_SIZE);
        for i in 0..actual_count {
            let start = i * BQ_SIZE;
            let end = start + BQ_SIZE;
            if end > data.len() {
                break;
            }
            if let Some(bq) = BinaryVector::from_bytes(&data[start..end]) {
                vectors[i] = bq;
            }
        }

        Ok(vectors)
    }
}

/// Reader for SQ8 shadow table chunks
pub struct SQ8ChunkReader<'a> {
    db: &'a SqliteDb<'a>,
    current_chunk_id: u32,
    current_data: Option<&'a [u8]>,
}

impl<'a> SQ8ChunkReader<'a> {
    /// Create a new SQ8 chunk reader
    pub fn new(db: &'a SqliteDb<'a>) -> Self {
        Self {
            db,
            current_chunk_id: u32::MAX,
            current_data: None,
        }
    }

    /// Load a chunk by ID
    fn load_chunk(&mut self, chunk_id: u32) -> Result<(), Error> {
        if self.current_chunk_id == chunk_id && self.current_data.is_some() {
            return Ok(());
        }

        let scanner = self.db.table_scan("shadow_sq8")?;
        for result in scanner {
            let record = result?;
            if let Some(Value::Integer(id)) = record.get(0) {
                if id as u32 == chunk_id {
                    if let Some(Value::Blob(data)) = record.get(1) {
                        self.current_chunk_id = chunk_id;
                        self.current_data = Some(data);
                        return Ok(());
                    }
                }
            }
        }

        Err(Error::TableNotFound)
    }

    /// Get an SQ8 vector from a specific chunk and offset
    pub fn get_vector(&mut self, chunk_id: u32, offset: u8) -> Result<ScalarVector, Error> {
        if offset as usize >= SQ8_CHUNK_SIZE {
            return Err(Error::InvalidRecord);
        }

        self.load_chunk(chunk_id)?;

        let data = self.current_data.ok_or(Error::InvalidRecord)?;
        let start = (offset as usize) * SQ8_SERIALIZED_SIZE;
        let end = start + SQ8_SERIALIZED_SIZE;

        if end > data.len() {
            return Err(Error::InvalidRecord);
        }

        ScalarVector::from_bytes(&data[start..end]).ok_or(Error::InvalidRecord)
    }
}

/// Check if shadow tables exist in the database
pub fn has_shadow_tables(db: &SqliteDb) -> bool {
    // Try to find shadow_bq in schema
    let has_bq = db.find_table_root("shadow_bq").is_ok();
    let has_sq8 = db.find_table_root("shadow_sq8").is_ok();
    let has_meta = db.find_table_root("synapse_meta_index").is_ok();

    has_bq && has_sq8 && has_meta
}

/// Iterator over vector metadata from synapse_meta_index
pub struct MetaIndexIterator<'a> {
    db: &'a SqliteDb<'a>,
    scanner: Option<crate::btree::TableScanner<'a, 'a>>,
}

impl<'a> MetaIndexIterator<'a> {
    /// Create a new metadata index iterator
    pub fn new(db: &'a SqliteDb<'a>) -> Result<Self, Error> {
        let scanner = db.table_scan("synapse_meta_index")?;
        Ok(Self {
            db,
            scanner: Some(scanner),
        })
    }

    /// Get the database reference
    #[allow(dead_code)]
    pub fn db(&self) -> &SqliteDb<'a> {
        self.db
    }
}

impl<'a> Iterator for MetaIndexIterator<'a> {
    type Item = Result<VectorMeta, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        let scanner = self.scanner.as_mut()?;

        loop {
            match scanner.next()? {
                Ok(record) => {
                    // synapse_meta_index: user_rowid, bq_chunk_id, bq_offset_idx, sq8_chunk_id, sq8_offset_idx
                    let user_rowid = match record.get(0) {
                        Some(Value::Integer(v)) => v as u32,
                        _ => continue,
                    };
                    let bq_chunk_id = match record.get(1) {
                        Some(Value::Integer(v)) => v as u32,
                        _ => continue,
                    };
                    let bq_offset = match record.get(2) {
                        Some(Value::Integer(v)) => v as u8,
                        _ => continue,
                    };
                    let sq8_chunk_id = match record.get(3) {
                        Some(Value::Integer(v)) => v as u32,
                        _ => continue,
                    };
                    let sq8_offset = match record.get(4) {
                        Some(Value::Integer(v)) => v as u8,
                        _ => continue,
                    };

                    return Some(Ok(VectorMeta {
                        user_rowid,
                        bq_chunk_id,
                        bq_offset,
                        sq8_chunk_id,
                        sq8_offset,
                    }));
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// Count total vectors in shadow tables
pub fn count_shadow_vectors(db: &SqliteDb) -> Result<usize, Error> {
    let scanner = db.table_scan("synapse_meta_index")?;
    Ok(scanner.filter(|r| r.is_ok()).count())
}

/// Candidate from BQ pass (for re-ranking)
#[derive(Clone, Copy, Debug, Default)]
pub struct BQCandidate {
    /// Metadata for this vector
    pub meta: VectorMeta,
    /// Hamming distance from query
    pub hamming_distance: u32,
}

/// Maximum candidates to track for re-ranking
pub const MAX_CANDIDATES: usize = 1000;

/// Candidate buffer for two-pass search
#[derive(Clone, Copy)]
pub struct CandidateBuffer {
    /// Candidates sorted by Hamming distance (ascending)
    pub candidates: [BQCandidate; MAX_CANDIDATES],
    /// Number of valid candidates
    pub count: usize,
}

impl Default for CandidateBuffer {
    fn default() -> Self {
        Self {
            candidates: [BQCandidate::default(); MAX_CANDIDATES],
            count: 0,
        }
    }
}

impl CandidateBuffer {
    /// Create a new empty buffer
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a candidate if it's better than the worst one
    ///
    /// # Arguments
    /// * `meta` - Vector metadata
    /// * `hamming_distance` - Hamming distance from query
    /// * `max_candidates` - Maximum candidates to keep
    pub fn insert(&mut self, meta: VectorMeta, hamming_distance: u32, max_candidates: usize) {
        let max_keep = max_candidates.min(MAX_CANDIDATES);

        // If buffer isn't full, always insert
        if self.count < max_keep {
            // Find insertion point (sorted ascending by distance)
            let mut pos = self.count;
            while pos > 0 && self.candidates[pos - 1].hamming_distance > hamming_distance {
                pos -= 1;
            }

            // Shift elements
            for i in (pos..self.count).rev() {
                self.candidates[i + 1] = self.candidates[i];
            }

            self.candidates[pos] = BQCandidate { meta, hamming_distance };
            self.count += 1;
            return;
        }

        // Buffer is full - only insert if better than worst
        if hamming_distance >= self.candidates[max_keep - 1].hamming_distance {
            return;
        }

        // Find insertion point
        let mut pos = max_keep - 1;
        while pos > 0 && self.candidates[pos - 1].hamming_distance > hamming_distance {
            pos -= 1;
        }

        // Shift elements (dropping the worst one)
        for i in (pos..max_keep - 1).rev() {
            self.candidates[i + 1] = self.candidates[i];
        }

        self.candidates[pos] = BQCandidate { meta, hamming_distance };
    }

    /// Get candidates slice
    pub fn as_slice(&self) -> &[BQCandidate] {
        &self.candidates[..self.count]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_candidate_buffer_insert() {
        let mut buffer = CandidateBuffer::new();

        // Insert some candidates
        buffer.insert(VectorMeta { user_rowid: 1, ..Default::default() }, 100, 5);
        buffer.insert(VectorMeta { user_rowid: 2, ..Default::default() }, 50, 5);
        buffer.insert(VectorMeta { user_rowid: 3, ..Default::default() }, 75, 5);

        assert_eq!(buffer.count, 3);

        // Should be sorted by distance ascending
        assert_eq!(buffer.candidates[0].hamming_distance, 50);
        assert_eq!(buffer.candidates[1].hamming_distance, 75);
        assert_eq!(buffer.candidates[2].hamming_distance, 100);
    }

    #[test]
    fn test_candidate_buffer_max_size() {
        let mut buffer = CandidateBuffer::new();

        // Insert 10 candidates with max 5
        for i in 0..10 {
            buffer.insert(
                VectorMeta { user_rowid: i, ..Default::default() },
                (10 - i) as u32 * 10,  // 100, 90, 80, 70, 60, 50, 40, 30, 20, 10
                5
            );
        }

        assert_eq!(buffer.count, 5);

        // Should keep the 5 smallest distances: 10, 20, 30, 40, 50
        assert_eq!(buffer.candidates[0].hamming_distance, 10);
        assert_eq!(buffer.candidates[1].hamming_distance, 20);
        assert_eq!(buffer.candidates[2].hamming_distance, 30);
        assert_eq!(buffer.candidates[3].hamming_distance, 40);
        assert_eq!(buffer.candidates[4].hamming_distance, 50);
    }

    #[test]
    fn test_candidate_buffer_reject_worse() {
        let mut buffer = CandidateBuffer::new();

        // Fill buffer with good candidates
        for i in 0..5 {
            buffer.insert(
                VectorMeta { user_rowid: i, ..Default::default() },
                i as u32 * 10,  // 0, 10, 20, 30, 40
                5
            );
        }

        // Try to insert a worse one
        buffer.insert(
            VectorMeta { user_rowid: 99, ..Default::default() },
            100,  // Worse than all existing
            5
        );

        // Should not be inserted
        assert_eq!(buffer.count, 5);
        for c in &buffer.candidates[..5] {
            assert_ne!(c.meta.user_rowid, 99);
        }
    }

    #[test]
    fn test_bq_chunk_constants() {
        // Verify chunk sizes fit in a 4KB page
        assert!(BQ_CHUNK_BYTES <= 4096);
        assert!(SQ8_CHUNK_BYTES <= 4096);

        // Verify expected sizes
        assert_eq!(BQ_CHUNK_BYTES, 64 * 48);  // 3072
        assert_eq!(SQ8_CHUNK_BYTES, 8 * 392); // 3136
    }
}
