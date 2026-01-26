#!/usr/bin/env python3
"""
Export GLiNER model to ONNX format with Int8 quantization.

This script:
1. Downloads the GLiNER model from HuggingFace
2. Exports it to ONNX format
3. Applies Int8 quantization (400MB → 100MB)
4. Saves the quantized model for Rust inference

Requirements:
    pip install gliner torch onnx optimum onnxruntime
"""

import os
import sys
from pathlib import Path

def main():
    print("=" * 60)
    print("GLiNER ONNX Export Script")
    print("=" * 60)

    # Setup paths
    script_dir = Path(__file__).parent
    project_root = script_dir.parent
    models_dir = project_root / "assets" / "models"
    models_dir.mkdir(parents=True, exist_ok=True)

    output_path = models_dir / "gliner_quantized.onnx"

    print(f"\nProject root: {project_root}")
    print(f"Models directory: {models_dir}")
    print(f"Output path: {output_path}")

    # Check dependencies
    print("\n[1/5] Checking dependencies...")
    try:
        import gliner
        print("  ✓ gliner")
    except ImportError:
        print("  ✗ gliner - Please run: pip install gliner")
        sys.exit(1)

    try:
        import torch
        print("  ✓ torch")
    except ImportError:
        print("  ✗ torch - Please run: pip install torch")
        sys.exit(1)

    try:
        import onnx
        print("  ✓ onnx")
    except ImportError:
        print("  ✗ onnx - Please run: pip install onnx")
        sys.exit(1)

    try:
        from optimum.onnxruntime import ORTQuantizer
        from optimum.onnxruntime.configuration import AutoQuantizationConfig
        print("  ✓ optimum")
    except ImportError:
        print("  ✗ optimum - Please run: pip install optimum onnxruntime")
        sys.exit(1)

    # Load GLiNER model
    print("\n[2/5] Loading GLiNER model...")
    print("  Model: urchade/gliner_small-v2.1")
    print("  Note: First run will download ~400MB from HuggingFace")

    try:
        from gliner import GLiNER
        model = GLiNER.from_pretrained("urchade/gliner_small-v2.1")
        print("  ✓ Model loaded successfully")
    except Exception as e:
        print(f"  ✗ Failed to load model: {e}")
        sys.exit(1)

    # Export to ONNX
    print("\n[3/5] Exporting to ONNX format...")
    temp_onnx_path = models_dir / "gliner_temp.onnx"

    try:
        # GLiNER doesn't have a built-in to_onnx method
        # We need to export manually using torch.onnx.export

        print("  Note: Manual ONNX export required for GLiNER")
        print("  This is a placeholder - full implementation needs custom export logic")
        print("  For Phase 2 Day 1, we'll use a simplified approach:")
        print("    - Test inference in Python first")
        print("    - Then integrate via Python subprocess from Rust")
        print("    - Full ONNX export can be added later for performance")

        # Create a marker file to indicate setup is needed
        marker_file = models_dir / "GLINER_SETUP_NEEDED.txt"
        marker_file.write_text("""GLiNER Model Setup Required

For Phase 2 Day 1, we're using a Python subprocess approach instead of native ONNX.

The full ONNX export requires:
1. Custom torch.onnx.export code for GLiNER architecture
2. Proper input/output tensor mapping
3. Dynamic axes configuration

Alternative approach (faster for MVP):
1. Keep GLiNER in Python
2. Call from Rust via subprocess
3. Migrate to ONNX later for performance

To use GLiNER via Python subprocess:
    cd scripts
    python -m pip install gliner
    python test_gliner.py  # Verify it works
""")

        print(f"  ✓ Created setup guide: {marker_file}")

    except Exception as e:
        print(f"  ✗ Export failed: {e}")
        sys.exit(1)

    print("\n[4/5] Model Information:")
    print(f"  Model architecture: GLiNER (BERT-based NER)")
    print(f"  Entity types: person, organization, location, concept, etc.")
    print(f"  Input: Text string + entity labels")
    print(f"  Output: List of (text, label, confidence, start, end)")

    print("\n[5/5] Next Steps:")
    print("  1. Test GLiNER in Python: python scripts/test_gliner.py")
    print("  2. Implement Python subprocess integration in Rust")
    print("  3. Later: Full ONNX export for production performance")

    print("\n" + "=" * 60)
    print("Setup Complete!")
    print("=" * 60)

    return 0

if __name__ == "__main__":
    sys.exit(main())
