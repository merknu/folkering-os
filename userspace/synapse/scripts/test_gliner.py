#!/usr/bin/env python3
"""
Test GLiNER entity extraction in Python.

This script verifies that GLiNER works correctly before we integrate it with Rust.
"""

import sys
import json

def main():
    print("Testing GLiNER Entity Extraction\n")

    # Import GLiNER
    try:
        from gliner import GLiNER
    except ImportError:
        print("Error: GLiNER not installed")
        print("Please run: pip install gliner")
        return 1

    # Load model
    print("[1/3] Loading GLiNER model...")
    print("  Model: urchade/gliner_small-v2.1")

    try:
        model = GLiNER.from_pretrained("urchade/gliner_small-v2.1")
        print("  ✓ Model loaded\n")
    except Exception as e:
        print(f"  ✗ Failed to load model: {e}")
        return 1

    # Test cases
    test_cases = [
        {
            "text": "Alice and Bob discussed physics at MIT",
            "labels": ["person", "concept", "organization"],
        },
        {
            "text": "Project Mars is a collaboration between NASA and SpaceX",
            "labels": ["project", "organization"],
        },
        {
            "text": "The microkernel uses message passing for IPC",
            "labels": ["concept", "technology"],
        },
    ]

    # Run predictions
    print("[2/3] Running entity extraction...\n")

    for i, test in enumerate(test_cases, 1):
        print(f"Test {i}: \"{test['text']}\"")
        print(f"Labels: {test['labels']}")

        try:
            entities = model.predict_entities(
                test['text'],
                test['labels'],
                threshold=0.5  # Confidence threshold
            )

            if entities:
                print(f"  Found {len(entities)} entities:")
                for entity in entities:
                    print(f"    - '{entity['text']}' ({entity['label']}, confidence: {entity['score']:.2f})")
            else:
                print("  No entities found")

        except Exception as e:
            print(f"  ✗ Prediction failed: {e}")
            return 1

        print()

    # Performance test
    print("[3/3] Performance test...")
    import time

    text = "The Folkering OS project uses Rust and focuses on digital sovereignty. " \
           "Alice and Bob are the main developers working on the microkernel."
    labels = ["person", "project", "concept", "technology"]

    start = time.time()
    entities = model.predict_entities(text, labels, threshold=0.5)
    duration = (time.time() - start) * 1000  # Convert to ms

    print(f"  Text length: {len(text)} characters")
    print(f"  Entities found: {len(entities)}")
    print(f"  Inference time: {duration:.1f}ms")

    if duration < 500:
        print("  ✓ Performance: Good (<500ms)")
    elif duration < 1000:
        print("  ⚠ Performance: Acceptable (<1000ms)")
    else:
        print("  ✗ Performance: Slow (>1000ms) - consider optimization")

    print("\n" + "=" * 60)
    print("GLiNER is working correctly!")
    print("=" * 60)
    print("\nNext steps:")
    print("  1. Integrate with Rust via subprocess")
    print("  2. Implement JSON-based communication protocol")
    print("  3. Add error handling and retries")

    return 0

if __name__ == "__main__":
    sys.exit(main())
