#!/usr/bin/env python3
"""
Embedding generation service for Synapse.

This script provides a subprocess interface for generating text embeddings
using sentence-transformers. It communicates via JSON over stdin/stdout.

Model: sentence-transformers/all-MiniLM-L6-v2
- 384-dimensional embeddings
- ~80MB model size
- Fast inference (~50-100ms per text)

Communication Protocol:
- Input (stdin): {"text": "your text here"}
- Output (stdout): {"embedding": [0.1, 0.2, ...], "error": null}

Error handling:
- Returns {"embedding": null, "error": "error message"} on failure
"""

import sys
import json
import logging
from typing import Optional, List

# Suppress TensorFlow warnings
import os
os.environ['TF_CPP_MIN_LOG_LEVEL'] = '3'

# Configure logging
logging.basicConfig(
    level=logging.ERROR,
    format='%(asctime)s - %(levelname)s - %(message)s',
    stream=sys.stderr
)

class EmbeddingService:
    """Sentence transformer embedding service."""

    def __init__(self, model_name: str = "sentence-transformers/all-MiniLM-L6-v2"):
        """Initialize the embedding model.

        Args:
            model_name: HuggingFace model identifier
        """
        try:
            from sentence_transformers import SentenceTransformer
            logging.info(f"Loading model: {model_name}")
            self.model = SentenceTransformer(model_name)
            logging.info("Model loaded successfully")
        except ImportError:
            logging.error("sentence-transformers not installed")
            raise ImportError(
                "sentence-transformers not installed. "
                "Install with: pip install sentence-transformers"
            )
        except Exception as e:
            logging.error(f"Failed to load model: {e}")
            raise

    def generate_embedding(self, text: str) -> Optional[List[float]]:
        """Generate embedding for text.

        Args:
            text: Input text (will be truncated to model's max length)

        Returns:
            List of floats (384-dimensional for MiniLM-L6-v2), or None on error
        """
        try:
            # Generate embedding
            embedding = self.model.encode(text, convert_to_numpy=True)

            # Convert to list of floats
            embedding_list = embedding.tolist()

            logging.debug(f"Generated embedding: dim={len(embedding_list)}")
            return embedding_list

        except Exception as e:
            logging.error(f"Embedding generation failed: {e}")
            return None


def main():
    """Main subprocess loop."""
    try:
        # Initialize service
        service = EmbeddingService()

        # Signal ready
        sys.stderr.write("Embedding service ready\n")
        sys.stderr.flush()

        # Process requests from stdin
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue

            try:
                # Parse request
                request = json.loads(line)
                text = request.get("text", "")

                if not text:
                    response = {
                        "embedding": None,
                        "error": "Empty text provided"
                    }
                else:
                    # Generate embedding
                    embedding = service.generate_embedding(text)

                    if embedding is not None:
                        response = {
                            "embedding": embedding,
                            "error": None
                        }
                    else:
                        response = {
                            "embedding": None,
                            "error": "Embedding generation failed"
                        }

                # Send response
                print(json.dumps(response), flush=True)

            except json.JSONDecodeError as e:
                error_response = {
                    "embedding": None,
                    "error": f"Invalid JSON: {e}"
                }
                print(json.dumps(error_response), flush=True)

            except Exception as e:
                error_response = {
                    "embedding": None,
                    "error": f"Unexpected error: {e}"
                }
                print(json.dumps(error_response), flush=True)

    except Exception as e:
        logging.error(f"Fatal error: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()
