//! Benchmark: Brute-force vs Quantized Vector Search
//!
//! Run with: cargo bench --features bench
//! Or for a simple timing test: cargo test --release bench_comparison -- --nocapture

#![cfg(test)]

use std::time::Instant;

// Import from our library
use crate::vector::{Embedding, EMBEDDING_DIM};
use crate::quantize::{quantize_binary, quantize_scalar, BinaryVector, ScalarVector};
use crate::simd::{detect_cpu_features, hamming_distance, l2_squared};

/// Number of vectors in the test dataset
const NUM_VECTORS: usize = 1000;

/// Number of search queries to run
const NUM_QUERIES: usize = 100;

/// Top-k results to retrieve
const TOP_K: usize = 10;

/// Overselection factor for BQ pass
const OVERSELECTION: usize = 10;

/// Generate a random-ish embedding (deterministic based on seed)
fn generate_embedding(seed: usize) -> Embedding {
    let mut embedding = Embedding::default();
    for i in 0..EMBEDDING_DIM {
        // Simple pseudo-random based on seed and index
        let val = ((seed * 7919 + i * 104729) % 10000) as f32 / 5000.0 - 1.0;
        embedding.values[i] = val;
    }
    // Normalize
    let norm: f32 = embedding.values.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut embedding.values {
            *v /= norm;
        }
    }
    embedding
}

/// Brute-force search simulation (what we had before)
fn brute_force_search(
    query: &Embedding,
    database: &[Embedding],
    k: usize,
) -> Vec<(usize, f32)> {
    let mut results: Vec<(usize, f32)> = database
        .iter()
        .enumerate()
        .map(|(idx, emb)| (idx, query.cosine_similarity(emb)))
        .collect();

    // Sort by similarity descending
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    results.truncate(k);
    results
}

/// Two-pass quantized search simulation
fn quantized_search(
    query: &Embedding,
    bq_database: &[BinaryVector],
    sq_database: &[ScalarVector],
    k: usize,
    overselection: usize,
) -> Vec<(usize, i32)> {
    let cpu_features = detect_cpu_features();

    // Quantize query
    let query_bq = quantize_binary(query);
    let query_sq = quantize_scalar(query);

    let num_candidates = (k * overselection).min(bq_database.len());

    // === Pass 1: BQ Hamming distance ===
    let mut candidates: Vec<(usize, u32)> = bq_database
        .iter()
        .enumerate()
        .map(|(idx, bq)| (idx, hamming_distance(&query_bq, bq, &cpu_features)))
        .collect();

    // Sort by Hamming distance ascending (lower = more similar)
    candidates.sort_by_key(|x| x.1);
    candidates.truncate(num_candidates);

    // === Pass 2: SQ8 L2 re-ranking ===
    let mut results: Vec<(usize, i32)> = candidates
        .iter()
        .map(|(idx, _)| {
            let l2_dist = l2_squared(&query_sq, &sq_database[*idx], &cpu_features);
            (*idx, l2_dist)
        })
        .collect();

    // Sort by L2 distance ascending
    results.sort_by_key(|x| x.1);
    results.truncate(k);
    results
}

/// Calculate recall: how many of the true top-k are in the approximate top-k
fn calculate_recall(
    true_results: &[(usize, f32)],
    approx_results: &[(usize, i32)],
) -> f64 {
    let true_ids: std::collections::HashSet<usize> =
        true_results.iter().map(|(idx, _)| *idx).collect();
    let approx_ids: std::collections::HashSet<usize> =
        approx_results.iter().map(|(idx, _)| *idx).collect();

    let intersection = true_ids.intersection(&approx_ids).count();
    intersection as f64 / true_results.len() as f64
}

#[test]
fn bench_comparison() {
    println!("\n============================================================");
    println!("Vector Search Performance Benchmark");
    println!("============================================================");
    println!("Dataset size: {} vectors", NUM_VECTORS);
    println!("Embedding dimension: {}", EMBEDDING_DIM);
    println!("Number of queries: {}", NUM_QUERIES);
    println!("Top-k: {}", TOP_K);
    println!("Overselection factor: {}x", OVERSELECTION);

    // Detect CPU features
    let cpu_features = detect_cpu_features();
    println!("\nCPU Features:");
    println!("  AVX2: {}", cpu_features.avx2);
    println!("  POPCNT: {}", cpu_features.popcnt);

    // Generate database
    println!("\nGenerating {} embeddings...", NUM_VECTORS);
    let start = Instant::now();
    let database: Vec<Embedding> = (0..NUM_VECTORS)
        .map(|i| generate_embedding(i))
        .collect();
    println!("  Generated in {:?}", start.elapsed());

    // Pre-quantize database (simulates shadow tables)
    println!("\nQuantizing database...");
    let start = Instant::now();
    let bq_database: Vec<BinaryVector> = database
        .iter()
        .map(|e| quantize_binary(e))
        .collect();
    let sq_database: Vec<ScalarVector> = database
        .iter()
        .map(|e| quantize_scalar(e))
        .collect();
    let quantize_time = start.elapsed();
    println!("  Quantized in {:?}", quantize_time);

    // Calculate storage sizes
    let f32_size = NUM_VECTORS * EMBEDDING_DIM * 4;
    let bq_size = NUM_VECTORS * 48;
    let sq_size = NUM_VECTORS * 392;
    println!("\nStorage comparison:");
    println!("  Original f32: {} bytes ({:.1} KB)", f32_size, f32_size as f64 / 1024.0);
    println!("  BQ (48B/vec): {} bytes ({:.1} KB) - {:.1}x compression",
             bq_size, bq_size as f64 / 1024.0, f32_size as f64 / bq_size as f64);
    println!("  SQ8 (392B/vec): {} bytes ({:.1} KB) - {:.1}x compression",
             sq_size, sq_size as f64 / 1024.0, f32_size as f64 / sq_size as f64);

    // Generate queries
    let queries: Vec<Embedding> = (NUM_VECTORS..NUM_VECTORS + NUM_QUERIES)
        .map(|i| generate_embedding(i))
        .collect();

    // Benchmark brute-force
    println!("\n--- Brute-Force Search (f32 cosine similarity) ---");
    let start = Instant::now();
    let mut brute_results = Vec::new();
    for query in &queries {
        brute_results.push(brute_force_search(query, &database, TOP_K));
    }
    let brute_time = start.elapsed();
    let brute_per_query = brute_time.as_micros() as f64 / NUM_QUERIES as f64;
    println!("  Total time: {:?}", brute_time);
    println!("  Per query: {:.1} µs", brute_per_query);
    println!("  Throughput: {:.0} queries/sec", 1_000_000.0 / brute_per_query);

    // Benchmark quantized search
    println!("\n--- Quantized 2-Pass Search (BQ Hamming -> SQ8 L2) ---");
    let start = Instant::now();
    let mut quant_results = Vec::new();
    for query in &queries {
        quant_results.push(quantized_search(query, &bq_database, &sq_database, TOP_K, OVERSELECTION));
    }
    let quant_time = start.elapsed();
    let quant_per_query = quant_time.as_micros() as f64 / NUM_QUERIES as f64;
    println!("  Total time: {:?}", quant_time);
    println!("  Per query: {:.1} µs", quant_per_query);
    println!("  Throughput: {:.0} queries/sec", 1_000_000.0 / quant_per_query);

    // Calculate speedup
    let speedup = brute_per_query / quant_per_query;
    println!("\n--- Performance Comparison ---");
    println!("  Speedup: {:.1}x faster", speedup);

    // Calculate recall
    let mut total_recall = 0.0;
    for (brute, quant) in brute_results.iter().zip(quant_results.iter()) {
        total_recall += calculate_recall(brute, quant);
    }
    let avg_recall = total_recall / NUM_QUERIES as f64;
    println!("  Average recall@{}: {:.1}%", TOP_K, avg_recall * 100.0);

    // Breakdown of quantized search
    println!("\n--- Quantized Search Breakdown ---");

    // Time BQ pass alone
    let start = Instant::now();
    for query in &queries {
        let query_bq = quantize_binary(query);
        let mut candidates: Vec<(usize, u32)> = bq_database
            .iter()
            .enumerate()
            .map(|(idx, bq)| (idx, hamming_distance(&query_bq, bq, &cpu_features)))
            .collect();
        candidates.sort_by_key(|x| x.1);
        candidates.truncate(TOP_K * OVERSELECTION);
    }
    let bq_time = start.elapsed();
    println!("  BQ pass (scan {} vectors): {:?} ({:.1} µs/query)",
             NUM_VECTORS, bq_time, bq_time.as_micros() as f64 / NUM_QUERIES as f64);

    // Time SQ8 pass alone
    let start = Instant::now();
    for query in &queries {
        let query_sq = quantize_scalar(query);
        let candidates: Vec<usize> = (0..TOP_K * OVERSELECTION).collect();
        let mut results: Vec<(usize, i32)> = candidates
            .iter()
            .map(|&idx| (idx, l2_squared(&query_sq, &sq_database[idx], &cpu_features)))
            .collect();
        results.sort_by_key(|x| x.1);
    }
    let sq_time = start.elapsed();
    println!("  SQ8 pass (re-rank {} candidates): {:?} ({:.1} µs/query)",
             TOP_K * OVERSELECTION, sq_time, sq_time.as_micros() as f64 / NUM_QUERIES as f64);

    println!("\n============================================================");
    println!("Summary: {:.1}x speedup with {:.1}% recall", speedup, avg_recall * 100.0);
    println!("============================================================\n");

    // Assert reasonable performance
    assert!(speedup > 1.0, "Quantized search should be faster than brute-force");
    assert!(avg_recall > 0.5, "Recall should be reasonable (>50%)");
}

#[test]
fn bench_scaling() {
    println!("\n============================================================");
    println!("Scaling Benchmark: How search time scales with dataset size");
    println!("============================================================\n");

    let cpu_features = detect_cpu_features();
    let sizes = [100, 250, 500, 1000, 2000];
    let num_queries = 20;

    println!("{:>8} | {:>12} | {:>12} | {:>8}", "Size", "Brute (µs)", "Quant (µs)", "Speedup");
    println!("---------+--------------+--------------+---------");

    for &size in &sizes {
        // Generate data
        let database: Vec<Embedding> = (0..size).map(|i| generate_embedding(i)).collect();
        let bq_database: Vec<BinaryVector> = database.iter().map(|e| quantize_binary(e)).collect();
        let sq_database: Vec<ScalarVector> = database.iter().map(|e| quantize_scalar(e)).collect();
        let queries: Vec<Embedding> = (size..size + num_queries).map(|i| generate_embedding(i)).collect();

        // Brute force
        let start = Instant::now();
        for query in &queries {
            let _ = brute_force_search(query, &database, TOP_K);
        }
        let brute_us = start.elapsed().as_micros() as f64 / num_queries as f64;

        // Quantized
        let start = Instant::now();
        for query in &queries {
            let _ = quantized_search(query, &bq_database, &sq_database, TOP_K, OVERSELECTION);
        }
        let quant_us = start.elapsed().as_micros() as f64 / num_queries as f64;

        let speedup = brute_us / quant_us;
        println!("{:>8} | {:>12.1} | {:>12.1} | {:>7.1}x", size, brute_us, quant_us, speedup);
    }

    println!();
}
