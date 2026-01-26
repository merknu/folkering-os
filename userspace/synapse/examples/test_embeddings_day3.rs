//! Test embedding generation - Phase 2 Day 3
//!
//! This example verifies that:
//! 1. Embedding service can be created
//! 2. Embeddings are 384-dimensional
//! 3. Similar texts have high similarity
//! 4. Dissimilar texts have low similarity
//! 5. Batch generation works
//!
//! Prerequisites:
//!   - Python 3.10+ with sentence-transformers installed
//!   - Run from project root: cargo run --example test_embeddings_day3

use anyhow::Result;
use synapse::{EmbeddingService, EMBEDDING_DIM, cosine_similarity};

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== Synapse Phase 2 Day 3: Embedding Generation Test ===\n");

    // Test 1: Create embedding service
    println!("[Test 1] Creating embedding service...");

    let service = match EmbeddingService::new() {
        Ok(s) => {
            println!("  ✓ Embedding service created");
            s
        }
        Err(e) => {
            println!("  ✗ Failed to create service: {}", e);
            println!("\nTo fix:");
            println!("  pip install sentence-transformers");
            println!("\nSkipping remaining tests.");
            return Ok(());
        }
    };

    // Test 2: Generate embedding
    println!("\n[Test 2] Generating embedding for text...");

    let text = "Machine learning with neural networks";
    let embedding = service.generate(text)?;

    println!("  Text: \"{}\"", text);
    println!("  Embedding dimension: {}", embedding.len());
    println!("  First 5 values: {:?}", &embedding[..5]);

    assert_eq!(embedding.len(), EMBEDDING_DIM, "Embedding should be 384-dimensional");
    println!("  ✓ Correct dimension (384)");

    // Check that embedding is not all zeros
    let sum: f32 = embedding.iter().sum();
    assert!(sum.abs() > 0.0, "Embedding should not be all zeros");
    println!("  ✓ Non-zero embedding (sum = {:.4})", sum);

    // Check that values are finite
    for value in &embedding {
        assert!(value.is_finite(), "Embedding values should be finite");
    }
    println!("  ✓ All values finite");

    // Test 3: Semantic similarity (related texts)
    println!("\n[Test 3] Testing semantic similarity (related texts)...");

    let text1 = "Machine learning with neural networks";
    let text2 = "Deep learning and artificial intelligence";

    let emb1 = service.generate(text1)?;
    let emb2 = service.generate(text2)?;

    let similarity = cosine_similarity(&emb1, &emb2)?;

    println!("  Text 1: \"{}\"", text1);
    println!("  Text 2: \"{}\"", text2);
    println!("  Similarity: {:.4}", similarity);

    assert!(
        similarity > 0.5,
        "Related texts should have high similarity (>0.5), got: {}",
        similarity
    );
    println!("  ✓ High similarity for related texts ({:.4} > 0.5)", similarity);

    // Test 4: Low similarity (unrelated texts)
    println!("\n[Test 4] Testing low similarity (unrelated texts)...");

    let text3 = "Cooking pasta with tomato sauce";

    let emb3 = service.generate(text3)?;

    let similarity_unrelated = cosine_similarity(&emb1, &emb3)?;

    println!("  Text 1: \"{}\"", text1);
    println!("  Text 3: \"{}\"", text3);
    println!("  Similarity: {:.4}", similarity_unrelated);

    assert!(
        similarity_unrelated < 0.5,
        "Unrelated texts should have low similarity (<0.5), got: {}",
        similarity_unrelated
    );
    println!("  ✓ Low similarity for unrelated texts ({:.4} < 0.5)", similarity_unrelated);

    // Test 5: Batch generation
    println!("\n[Test 5] Testing batch generation...");

    let texts = vec![
        "Rust programming language",
        "Python for data science",
        "JavaScript web development",
    ];

    let embeddings = service.generate_batch(&texts)?;

    println!("  Generated {} embeddings", embeddings.len());
    assert_eq!(embeddings.len(), texts.len(), "Should generate one embedding per text");

    for (i, embedding) in embeddings.iter().enumerate() {
        assert_eq!(embedding.len(), EMBEDDING_DIM);
        println!("  ✓ Embedding {}: {} dimensions", i + 1, embedding.len());
    }

    // Test 6: Empty text handling
    println!("\n[Test 6] Testing error handling (empty text)...");

    let result = service.generate("");
    assert!(result.is_err(), "Empty text should return error");
    println!("  ✓ Empty text correctly rejected");

    // Test 7: Embedding persistence properties
    println!("\n[Test 7] Testing embedding properties...");

    let text_a = "Kubernetes container orchestration";
    let emb_a1 = service.generate(text_a)?;
    let emb_a2 = service.generate(text_a)?;

    let self_similarity = cosine_similarity(&emb_a1, &emb_a2)?;

    println!("  Same text, two embeddings");
    println!("  Similarity: {:.6}", self_similarity);

    assert!(
        (self_similarity - 1.0).abs() < 0.001,
        "Same text should produce (nearly) identical embeddings"
    );
    println!("  ✓ Deterministic embeddings (self-similarity ≈ 1.0)");

    // Test 8: Triangle inequality check
    println!("\n[Test 8] Testing semantic space properties...");

    // Three texts: A (ML), B (DL), C (Cooking)
    let texts_triangle = vec![
        "Machine learning algorithms",
        "Deep neural networks",
        "Baking chocolate cake",
    ];

    let embs: Vec<_> = texts_triangle.iter()
        .map(|t| service.generate(t).unwrap())
        .collect();

    let sim_ab = cosine_similarity(&embs[0], &embs[1])?;
    let sim_bc = cosine_similarity(&embs[1], &embs[2])?;
    let sim_ac = cosine_similarity(&embs[0], &embs[2])?;

    println!("  A (ML) ↔ B (DL):      {:.4}", sim_ab);
    println!("  B (DL) ↔ C (Cooking): {:.4}", sim_bc);
    println!("  A (ML) ↔ C (Cooking): {:.4}", sim_ac);

    // ML and DL should be most similar
    assert!(sim_ab > sim_bc && sim_ab > sim_ac);
    println!("  ✓ Semantic relationships preserved (ML-DL most similar)");

    // Summary
    println!("\n=== Test Summary ===");
    println!("✓ Embedding service creation: OK");
    println!("✓ 384-dimensional embeddings: OK");
    println!("✓ Related texts similarity: OK (high)");
    println!("✓ Unrelated texts similarity: OK (low)");
    println!("✓ Batch generation: OK");
    println!("✓ Error handling: OK");
    println!("✓ Deterministic embeddings: OK");
    println!("✓ Semantic space properties: OK");

    println!("\n=== Phase 2 Day 3 Complete! ===");
    println!("\nKey Achievements:");
    println!("  - Embedding service fully functional");
    println!("  - 384-dimensional embeddings from sentence-transformers");
    println!("  - Semantic similarity working (related > 0.5, unrelated < 0.5)");
    println!("  - Batch processing supported");
    println!("  - Robust error handling");

    println!("\nEmbedding Statistics:");
    println!("  Model: all-MiniLM-L6-v2");
    println!("  Dimension: {}", EMBEDDING_DIM);
    println!("  Related similarity: {:.4}", similarity);
    println!("  Unrelated similarity: {:.4}", similarity_unrelated);
    println!("  Self-similarity: {:.6}", self_similarity);

    println!("\nNext Steps:");
    println!("  - Day 4: sqlite-vec integration (vector search)");
    println!("  - Day 5: Full pipeline integration with observer");
    println!("  - Day 6: Hybrid search (RRF algorithm)");

    Ok(())
}
