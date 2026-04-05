"""Cryptographic Lineage — WASM binary signing.

Every LLM-generated WASM binary is signed with an intention signature:
SHA256(prompt + SHA256(wasm) + timestamp). The OS verifies this before execution.
"""

import hashlib
import time


def sign_wasm(wasm_bytes: bytes, prompt: str) -> bytes:
    """Sign WASM binary with cryptographic intention signature.
    Prepends a 37-byte header: FOLK\\x00 (5 bytes) + SHA256 signature (32 bytes).
    The OS strips and verifies this header before execution."""
    wasm_hash = hashlib.sha256(wasm_bytes).digest()
    timestamp = int(time.time()).to_bytes(8, 'little')
    prompt_bytes = prompt.encode('utf-8')[:4096]
    sig_input = prompt_bytes + wasm_hash + timestamp
    signature = hashlib.sha256(sig_input).digest()
    header = b'FOLK\x00' + signature
    print(f"[CRYPTO] Signed: {hashlib.sha256(wasm_bytes).hexdigest()[:16]}... sig={signature.hex()[:16]}...")
    return header + wasm_bytes
