//! A tiny `.fbin` blob hand-crafted for boot-time round-trip testing.
//!
//! Two tensors:
//!   - "embed_test"  shape (4, 4) f32  — values 1..16
//!   - "weight_test" shape (4,)   f32  — values 0.25, 0.5, 0.75, 1.0
//!
//! The blob is laid out per the `.fbin` spec in `weights.rs`, with
//! the data section page-aligned. Total ≈ 4.1 KiB.
//!
//! When D.3.1.2 plumbs Synapse VFS reads, the same byte layout will
//! land via `libfolk::sys::synapse::read_file` — we replace the
//! `&[u8]` const with a `Vec<u8>` from the file and the parser
//! doesn't change. That's the whole point of doing the format spec
//! before the I/O path.

/// Hand-built blob. Generated once by reading the spec; if it ever
/// drifts from the spec, the boot self-test fails immediately and
/// the regression is obvious.
pub const TEST_FBIN: &[u8] = &{
    let mut buf = [0u8; 4096 + 80];

    // ── Header (16 bytes) ──────────────────────────────────────────
    // magic = b"FBN1"
    buf[0] = b'F'; buf[1] = b'B'; buf[2] = b'N'; buf[3] = b'1';
    // version = 1 (LE u16)
    buf[4] = 1; buf[5] = 0;
    // n_tensors = 2 (LE u16)
    buf[6] = 2; buf[7] = 0;
    // metadata_len = 78 (computed below; written manually)
    // = 16 (entry 1) + 16 (entry 2) accounting:
    //   entry 1: name_len(2) + "embed_test"(10) + dtype(1) + rank(1)
    //          + shape(4*2=8) + data_offset(8) + data_len(8) = 38
    //   entry 2: name_len(2) + "weight_test"(11) + dtype(1) + rank(1)
    //          + shape(4*1=4) + data_offset(8) + data_len(8) = 35
    //   total = 73 bytes
    // (LE u64)
    buf[8]  = 73; buf[9]  = 0; buf[10] = 0; buf[11] = 0;
    buf[12] = 0;  buf[13] = 0; buf[14] = 0; buf[15] = 0;

    // ── Metadata for "embed_test" (4×4 f32) ────────────────────────
    let mut o = 16;
    // name_len = 10
    buf[o] = 10; buf[o+1] = 0; o += 2;
    // name = "embed_test"
    let n1 = b"embed_test";
    let mut k = 0;
    while k < n1.len() { buf[o + k] = n1[k]; k += 1; }
    o += 10;
    // dtype = 0 (F32)
    buf[o] = 0; o += 1;
    // rank = 2
    buf[o] = 2; o += 1;
    // shape = [4, 4]
    buf[o]   = 4; buf[o+1] = 0; buf[o+2] = 0; buf[o+3] = 0;
    buf[o+4] = 4; buf[o+5] = 0; buf[o+6] = 0; buf[o+7] = 0;
    o += 8;
    // data_offset = 4096 (start of data section, page-aligned)
    buf[o]   = 0;  buf[o+1] = 16; buf[o+2] = 0; buf[o+3] = 0;
    buf[o+4] = 0;  buf[o+5] = 0;  buf[o+6] = 0; buf[o+7] = 0;
    o += 8;
    // data_len = 64 (16 floats × 4 bytes)
    buf[o]   = 64; buf[o+1] = 0; buf[o+2] = 0; buf[o+3] = 0;
    buf[o+4] = 0;  buf[o+5] = 0; buf[o+6] = 0; buf[o+7] = 0;
    o += 8;

    // ── Metadata for "weight_test" (4 f32) ─────────────────────────
    // name_len = 11
    buf[o] = 11; buf[o+1] = 0; o += 2;
    let n2 = b"weight_test";
    let mut k = 0;
    while k < n2.len() { buf[o + k] = n2[k]; k += 1; }
    o += 11;
    // dtype = 0 (F32)
    buf[o] = 0; o += 1;
    // rank = 1
    buf[o] = 1; o += 1;
    // shape = [4]
    buf[o]   = 4; buf[o+1] = 0; buf[o+2] = 0; buf[o+3] = 0;
    o += 4;
    // data_offset = 4096 + 64 = 4160
    buf[o]   = 64; buf[o+1] = 16; buf[o+2] = 0; buf[o+3] = 0;
    buf[o+4] = 0;  buf[o+5] = 0;  buf[o+6] = 0; buf[o+7] = 0;
    o += 8;
    // data_len = 16 (4 floats × 4 bytes)
    buf[o]   = 16; buf[o+1] = 0; buf[o+2] = 0; buf[o+3] = 0;
    buf[o+4] = 0;  buf[o+5] = 0; buf[o+6] = 0; buf[o+7] = 0;
    let _ = o;
    // (no need to advance o — done with metadata)

    // ── Data section: starts at offset 4096 ────────────────────────
    // embed_test: 1.0, 2.0, ..., 16.0 (LE f32)
    let mut idx = 0;
    while idx < 16 {
        let v = (idx + 1) as f32;
        let b = v.to_le_bytes();
        let off = 4096 + idx * 4;
        buf[off]   = b[0];
        buf[off+1] = b[1];
        buf[off+2] = b[2];
        buf[off+3] = b[3];
        idx += 1;
    }
    // weight_test: 0.25, 0.5, 0.75, 1.0
    let weights: [f32; 4] = [0.25, 0.5, 0.75, 1.0];
    let mut idx = 0;
    while idx < 4 {
        let b = weights[idx].to_le_bytes();
        let off = 4096 + 64 + idx * 4;
        buf[off]   = b[0];
        buf[off+1] = b[1];
        buf[off+2] = b[2];
        buf[off+3] = b[3];
        idx += 1;
    }

    buf
};
