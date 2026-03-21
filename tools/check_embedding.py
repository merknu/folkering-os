"""
Read the Q8_0 token embedding from GGUF and dequantize token 1 (BOS).
Compare with Rust's embedding to find if the divergence starts at the embedding level.

Q8_0 block: 34 bytes = 2 bytes f16 scale + 32 × int8 values
dequant: value = scale * quant_value
"""
import struct
import numpy as np

GGUF_PATH = "boot/model.gguf"

def read_gguf_metadata(f):
    """Parse GGUF header to find tensor info."""
    magic = f.read(4)
    assert magic == b"GGUF", f"Bad magic: {magic}"
    version = struct.unpack("<I", f.read(4))[0]
    n_tensors = struct.unpack("<Q", f.read(8))[0]
    n_metadata = struct.unpack("<Q", f.read(8))[0]

    print(f"GGUF v{version}, {n_tensors} tensors, {n_metadata} metadata entries")

    # Skip metadata
    for _ in range(n_metadata):
        key_len = struct.unpack("<Q", f.read(8))[0]
        key = f.read(key_len).decode("utf-8", errors="replace")
        val_type = struct.unpack("<I", f.read(4))[0]
        skip_gguf_value(f, val_type)

    # Read tensor info
    tensors = {}
    for _ in range(n_tensors):
        name_len = struct.unpack("<Q", f.read(8))[0]
        name = f.read(name_len).decode("utf-8")
        n_dims = struct.unpack("<I", f.read(4))[0]
        dims = [struct.unpack("<Q", f.read(8))[0] for _ in range(n_dims)]
        dtype = struct.unpack("<I", f.read(4))[0]
        offset = struct.unpack("<Q", f.read(8))[0]
        tensors[name] = {"dims": dims, "dtype": dtype, "offset": offset}

    return tensors, f.tell()

def skip_gguf_value(f, val_type):
    """Skip a GGUF metadata value."""
    if val_type == 0:  # UINT8
        f.read(1)
    elif val_type == 1:  # INT8
        f.read(1)
    elif val_type == 2:  # UINT16
        f.read(2)
    elif val_type == 3:  # INT16
        f.read(2)
    elif val_type == 4:  # UINT32
        f.read(4)
    elif val_type == 5:  # INT32
        f.read(4)
    elif val_type == 6:  # FLOAT32
        f.read(4)
    elif val_type == 7:  # BOOL
        f.read(1)
    elif val_type == 8:  # STRING
        slen = struct.unpack("<Q", f.read(8))[0]
        f.read(slen)
    elif val_type == 9:  # ARRAY
        elem_type = struct.unpack("<I", f.read(4))[0]
        n_elems = struct.unpack("<Q", f.read(8))[0]
        for _ in range(n_elems):
            skip_gguf_value(f, elem_type)
    elif val_type == 10:  # UINT64
        f.read(8)
    elif val_type == 11:  # INT64
        f.read(8)
    elif val_type == 12:  # FLOAT64
        f.read(8)

def dequant_q8_0_row(data, row_idx, n_cols):
    """Dequantize one row of a Q8_0 tensor.

    Q8_0 block: 34 bytes = 2 bytes f16 scale + 32 × int8 values
    Block size: 32 values
    """
    BLOCK_SIZE = 32
    BLOCK_BYTES = 34  # 2 (f16 scale) + 32 (int8 values)
    blocks_per_row = n_cols // BLOCK_SIZE

    row_offset = row_idx * blocks_per_row * BLOCK_BYTES
    result = np.zeros(n_cols, dtype=np.float32)

    for b in range(blocks_per_row):
        block_start = row_offset + b * BLOCK_BYTES
        # f16 scale (2 bytes)
        scale_f16 = struct.unpack_from("<e", data, block_start)[0]
        # 32 × int8 values
        for i in range(BLOCK_SIZE):
            quant_val = struct.unpack_from("<b", data, block_start + 2 + i)[0]  # signed int8
            result[b * BLOCK_SIZE + i] = scale_f16 * quant_val

    return result

def dequant_q4_0_row(data, row_idx, n_cols):
    """Dequantize one row of a Q4_0 tensor.

    Q4_0 block: 18 bytes = 2 bytes f16 scale + 16 bytes (32 × 4-bit nibbles)
    Block size: 32 values
    """
    BLOCK_SIZE = 32
    BLOCK_BYTES = 18  # 2 (f16 scale) + 16 (packed nibbles)
    blocks_per_row = n_cols // BLOCK_SIZE

    row_offset = row_idx * blocks_per_row * BLOCK_BYTES
    result = np.zeros(n_cols, dtype=np.float32)

    for b in range(blocks_per_row):
        block_start = row_offset + b * BLOCK_BYTES
        scale_f16 = struct.unpack_from("<e", data, block_start)[0]
        for i in range(BLOCK_SIZE):
            byte_idx = block_start + 2 + i // 2
            byte_val = data[byte_idx]
            if i % 2 == 0:
                nibble = byte_val & 0x0F
            else:
                nibble = (byte_val >> 4) & 0x0F
            # nibble is 0-15, centered at 8
            result[b * BLOCK_SIZE + i] = scale_f16 * (nibble - 8)

    return result

def main():
    import os
    os.chdir(r"C:\Users\merkn\folkering\folkering-os")

    with open(GGUF_PATH, "rb") as f:
        tensors, header_end = read_gguf_metadata(f)

    # Find the alignment/data start
    # GGUF aligns tensor data to 32 bytes after the header
    alignment = 32
    data_start = (header_end + alignment - 1) // alignment * alignment

    print(f"\nHeader ends at: {header_end}")
    print(f"Data starts at: {data_start}")

    # Find token_embd.weight
    embd_info = tensors.get("token_embd.weight")
    if embd_info is None:
        print("ERROR: token_embd.weight not found!")
        print("Available tensors:", [k for k in tensors if 'embd' in k.lower() or 'embed' in k.lower()])
        return

    print(f"\ntoken_embd.weight:")
    print(f"  dims: {embd_info['dims']}")
    print(f"  dtype: {embd_info['dtype']} (8=Q8_0, 2=Q4_0)")
    print(f"  offset from data_start: {embd_info['offset']}")

    abs_offset = data_start + embd_info['offset']
    print(f"  absolute offset: {abs_offset}")

    # Read the raw data
    n_cols = embd_info['dims'][0]  # embed_dim = 576
    n_rows = embd_info['dims'][1]  # vocab_size = 49152
    print(f"  shape: [{n_rows}, {n_cols}] (row-major: each row = one token's embedding)")

    with open(GGUF_PATH, "rb") as f:
        f.seek(abs_offset)
        # Calculate total bytes
        if embd_info['dtype'] == 8:  # Q8_0
            block_bytes = 34
            blocks_per_row = n_cols // 32
            total_bytes = n_rows * blocks_per_row * block_bytes
            raw_data = f.read(total_bytes)
            print(f"  Q8_0: {blocks_per_row} blocks/row, {block_bytes} bytes/block, total {len(raw_data)} bytes")

            # Dequantize BOS row (token 1)
            bos_embed = dequant_q8_0_row(raw_data, 1, n_cols)
        elif embd_info['dtype'] == 2:  # Q4_0
            block_bytes = 18
            blocks_per_row = n_cols // 32
            total_bytes = n_rows * blocks_per_row * block_bytes
            raw_data = f.read(total_bytes)
            bos_embed = dequant_q4_0_row(raw_data, 1, n_cols)
        else:
            print(f"  Unknown dtype: {embd_info['dtype']}")
            return

    print(f"\n=== BOS (token 1) embedding ===")
    print(f"  shape: [{n_cols}]")
    print(f"  min: {bos_embed.min():.6f}")
    print(f"  max: {bos_embed.max():.6f}")
    print(f"  mean: {bos_embed.mean():.6f}")
    print(f"  First 32 values:")
    for i in range(0, 32, 8):
        vals = " ".join(f"{bos_embed[j]:10.6f}" for j in range(i, i+8))
        print(f"    [{i:3d}] {vals}")

    # Also check first bytes of the raw data for token 1
    if embd_info['dtype'] == 8:
        blocks_per_row = n_cols // 32
        row_start = 1 * blocks_per_row * 34
        print(f"\n  Raw bytes at token 1 offset ({row_start}):")
        first_block = raw_data[row_start:row_start+34]
        scale_f16 = struct.unpack_from("<e", first_block, 0)[0]
        print(f"    Block 0: scale_f16={scale_f16:.6f}")
        quants = [struct.unpack_from("<b", first_block, 2+i)[0] for i in range(32)]
        print(f"    Quants: {quants[:16]}")
        dequant = [scale_f16 * q for q in quants[:16]]
        print(f"    Dequant: {[f'{v:.6f}' for v in dequant]}")

    # Also check what Wq (layer 0) looks like
    wq0_info = tensors.get("blk.0.attn_q.weight")
    if wq0_info:
        print(f"\n=== blk.0.attn_q.weight ===")
        print(f"  dims: {wq0_info['dims']}")
        print(f"  dtype: {wq0_info['dtype']}")
        q4_abs = data_start + wq0_info['offset']
        print(f"  absolute offset: {q4_abs}")
        with open(GGUF_PATH, "rb") as f:
            f.seek(q4_abs)
            first_bytes = f.read(4)
            print(f"  First 4 bytes: [{','.join(f'{b:02X}' for b in first_bytes)}]")

if __name__ == "__main__":
    main()
