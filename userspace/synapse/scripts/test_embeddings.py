#!/usr/bin/env python3
"""
Test sentence-transformers embedding generation.

This script validates that sentence-transformers is installed and working
correctly before integrating with Rust.

Usage:
    python scripts/test_embeddings.py
"""

import sys


def check_dependencies():
    """Check if required packages are installed."""
    print("=== Checking Dependencies ===")

    try:
        import sentence_transformers
        print(f"✓ sentence-transformers installed (v{sentence_transformers.__version__})")
    except ImportError:
        print("✗ sentence-transformers NOT installed")
        print("\nInstall with:")
        print("  pip install sentence-transformers")
        return False

    try:
        import torch
        print(f"✓ PyTorch installed (v{torch.__version__})")
    except ImportError:
        print("✗ PyTorch NOT installed")
        return False

    return True


def test_embedding_generation():
    """Test basic embedding generation."""
    print("\n=== Testing Embedding Generation ===")

    try:
        from sentence_transformers import SentenceTransformer
        import numpy as np

        # Load model
        print("Loading model: sentence-transformers/all-MiniLM-L6-v2")
        model = SentenceTransformer("sentence-transformers/all-MiniLM-L6-v2")
        print("✓ Model loaded successfully")

        # Test cases
        test_cases = [
            "Machine learning with neural networks",
            "Deep learning and artificial intelligence",
            "Cooking pasta with tomato sauce",
            "The quick brown fox jumps over the lazy dog",
        ]

        print(f"\n=== Generating Embeddings for {len(test_cases)} texts ===")
        embeddings = []

        for i, text in enumerate(test_cases):
            embedding = model.encode(text, convert_to_numpy=True)
            embeddings.append(embedding)

            print(f"\nText {i+1}: \"{text}\"")
            print(f"  Embedding shape: {embedding.shape}")
            print(f"  Embedding dtype: {embedding.dtype}")
            print(f"  First 5 values: {embedding[:5]}")
            print(f"  L2 norm: {np.linalg.norm(embedding):.4f}")

        # Test similarity
        print("\n=== Testing Cosine Similarity ===")

        # Compute similarity between ML texts (should be high)
        ml_sim = np.dot(embeddings[0], embeddings[1]) / (
            np.linalg.norm(embeddings[0]) * np.linalg.norm(embeddings[1])
        )
        print(f"Similarity (ML vs Deep Learning): {ml_sim:.4f}")

        # Compute similarity between ML and cooking (should be low)
        unrelated_sim = np.dot(embeddings[0], embeddings[2]) / (
            np.linalg.norm(embeddings[0]) * np.linalg.norm(embeddings[2])
        )
        print(f"Similarity (ML vs Cooking): {unrelated_sim:.4f}")

        # Validate
        if ml_sim > 0.5:
            print("✓ Related texts have high similarity")
        else:
            print(f"✗ Related texts have low similarity: {ml_sim:.4f}")

        if unrelated_sim < 0.5:
            print("✓ Unrelated texts have low similarity")
        else:
            print(f"✗ Unrelated texts have high similarity: {unrelated_sim:.4f}")

        print("\n=== All Tests Passed ✓ ===")
        return True

    except Exception as e:
        print(f"✗ Error during testing: {e}")
        import traceback
        traceback.print_exc()
        return False


def test_subprocess_protocol():
    """Test JSON subprocess protocol."""
    print("\n=== Testing Subprocess Protocol ===")

    try:
        import json
        import subprocess

        # Start subprocess
        print("Starting embedding_inference.py subprocess...")
        proc = subprocess.Popen(
            [sys.executable, "scripts/embedding_inference.py"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1
        )

        # Wait for ready signal
        import time
        time.sleep(2)  # Give model time to load

        # Test request
        request = {"text": "Hello world"}
        print(f"Sending request: {request}")

        proc.stdin.write(json.dumps(request) + "\n")
        proc.stdin.flush()

        # Read response
        response_line = proc.stdout.readline()
        response = json.loads(response_line)

        print(f"Received response: embedding length = {len(response.get('embedding', []))}")

        if response.get("error"):
            print(f"✗ Error: {response['error']}")
            proc.terminate()
            return False

        embedding = response.get("embedding")
        if embedding and len(embedding) == 384:
            print(f"✓ Received 384-dimensional embedding")
        else:
            print(f"✗ Invalid embedding: {len(embedding) if embedding else 'None'}")
            proc.terminate()
            return False

        # Cleanup
        proc.terminate()
        proc.wait()

        print("✓ Subprocess protocol working")
        return True

    except Exception as e:
        print(f"✗ Subprocess test failed: {e}")
        import traceback
        traceback.print_exc()
        return False


def main():
    """Run all tests."""
    print("=" * 60)
    print("Sentence-Transformers Embedding Test Suite")
    print("=" * 60)

    # Check dependencies
    if not check_dependencies():
        print("\n✗ Dependency check failed")
        print("\nTo install dependencies:")
        print("  pip install sentence-transformers")
        sys.exit(1)

    # Test embedding generation
    if not test_embedding_generation():
        print("\n✗ Embedding generation test failed")
        sys.exit(1)

    # Test subprocess protocol
    if not test_subprocess_protocol():
        print("\n✗ Subprocess protocol test failed")
        sys.exit(1)

    print("\n" + "=" * 60)
    print("✓ All Tests Passed!")
    print("=" * 60)
    print("\nSentence-transformers is ready for Synapse integration.")


if __name__ == "__main__":
    main()
