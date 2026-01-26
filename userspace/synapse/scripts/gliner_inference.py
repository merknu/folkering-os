#!/usr/bin/env python3
"""
GLiNER inference script for Rust subprocess integration.

This script:
1. Reads JSON request from stdin
2. Loads GLiNER model (cached after first load)
3. Extracts entities
4. Writes JSON response to stdout

Communication protocol:
    Input (stdin):  {"text": "...", "labels": [...], "threshold": 0.5}
    Output (stdout): {"entities": [...], "error": null}
"""

import sys
import json
import os

# Global model cache (loaded once, reused for all requests)
_MODEL_CACHE = None

def load_model():
    """Load GLiNER model (cached)"""
    global _MODEL_CACHE

    if _MODEL_CACHE is not None:
        return _MODEL_CACHE

    try:
        from gliner import GLiNER
    except ImportError:
        return {"error": "GLiNER not installed. Run: pip install gliner"}

    try:
        # Load model (downloads on first run)
        model = GLiNER.from_pretrained("urchade/gliner_small-v2.1")
        _MODEL_CACHE = model
        return model
    except Exception as e:
        return {"error": f"Failed to load GLiNER model: {str(e)}"}

def extract_entities(text, labels, threshold=0.5):
    """Extract entities using GLiNER"""
    # Load model
    model = load_model()

    # Check if loading failed
    if isinstance(model, dict) and "error" in model:
        return model

    try:
        # Run prediction
        raw_entities = model.predict_entities(
            text,
            labels,
            threshold=threshold
        )

        # Convert to our format
        entities = []
        for entity in raw_entities:
            entities.append({
                "text": entity["text"],
                "label": entity["label"],
                "confidence": float(entity["score"]),
                "start": int(entity["start"]),
                "end": int(entity["end"]),
            })

        return {"entities": entities, "error": None}

    except Exception as e:
        return {"error": f"Entity extraction failed: {str(e)}", "entities": []}

def main():
    """Main entry point"""
    # Disable buffering for immediate output
    sys.stdout.reconfigure(line_buffering=True)

    # Read request from stdin
    try:
        request_json = sys.stdin.read()
        request = json.loads(request_json)
    except json.JSONDecodeError as e:
        response = {"error": f"Invalid JSON input: {str(e)}", "entities": []}
        print(json.dumps(response))
        return 1
    except Exception as e:
        response = {"error": f"Failed to read input: {str(e)}", "entities": []}
        print(json.dumps(response))
        return 1

    # Validate request
    if "text" not in request or "labels" not in request:
        response = {"error": "Missing 'text' or 'labels' in request", "entities": []}
        print(json.dumps(response))
        return 1

    text = request["text"]
    labels = request["labels"]
    threshold = request.get("threshold", 0.5)

    # Extract entities
    response = extract_entities(text, labels, threshold)

    # Write response to stdout
    print(json.dumps(response))

    return 0 if response.get("error") is None else 1

if __name__ == "__main__":
    sys.exit(main())
