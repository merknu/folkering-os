//! Debug tensor dump (disk mailbox @ sectors 1-257) + health telemetry (sector 259).
//!
//! Two extraction paths for the host-side MCP tool:
//!   1. Serial log: [TDMP] lines with stats (always available)
//!   2. Disk mailbox: sectors 1-257 with raw f32 data (128KB, for attention/logits)

use libfolk::println;
use libfolk::sys::block::SECTOR_SIZE;

use crate::consts::{DUMP_HEADER_SECTOR, DUMP_DATA_SECTOR, DUMP_MAX_FLOATS, DUMP_MAX_SECTORS, HEALTH_SECTOR};

/// Monotonic sequence counter for [TDMP] log lines.
static mut DUMP_SEQ: u32 = 0;

/// Dump a named f32 tensor: print stats to serial AND write to disk mailbox.
///
/// Disk mailbox layout:
///   Sector 1 (header): magic, seq, shape, stats, name, first 100 f32 summary
///   Sectors 2-257 (data): raw f32 values, up to 32768 floats (128KB)
pub fn debug_dump_tensor(name: &str, data: &[f32], shape0: u32, shape1: u32) {
    let n = data.len();
    if n == 0 { return; }

    // Compute stats
    let mut min_val = data[0];
    let mut max_val = data[0];
    let mut sum = 0.0f64;
    let mut argmax_idx = 0u32;
    let mut argmax_val = data[0];

    for i in 0..n {
        let v = data[i];
        if v < min_val { min_val = v; }
        if v > max_val { max_val = v; argmax_idx = i as u32; argmax_val = v; }
        sum += v as f64;
    }
    let mean = (sum / n as f64) as f32;

    let seq = unsafe {
        DUMP_SEQ += 1;
        DUMP_SEQ
    };

    // Print to serial (always available — MCP tool parses this)
    println!("[TDMP] seq={} name={} shape=[{},{}] n={} argmax={}({:.6}) min={:.6} max={:.6} mean={:.6}",
        seq, name, shape0, shape1, n, argmax_idx, argmax_val, min_val, max_val, mean);

    // Write to disk mailbox for full float data extraction
    let n_dumped = n.min(DUMP_MAX_FLOATS) as u32;

    // Build header sector (512 bytes)
    let mut hdr = [0u8; SECTOR_SIZE];
    hdr[0..4].copy_from_slice(b"TDMP");
    hdr[4..8].copy_from_slice(&seq.to_le_bytes());
    hdr[8..12].copy_from_slice(&(n as u32).to_le_bytes());
    hdr[12..16].copy_from_slice(&n_dumped.to_le_bytes());
    hdr[16..20].copy_from_slice(&shape0.to_le_bytes());
    hdr[20..24].copy_from_slice(&shape1.to_le_bytes());
    hdr[24..28].copy_from_slice(&argmax_idx.to_le_bytes());
    hdr[32..36].copy_from_slice(&min_val.to_le_bytes());
    hdr[36..40].copy_from_slice(&max_val.to_le_bytes());
    hdr[40..44].copy_from_slice(&mean.to_le_bytes());
    hdr[44..48].copy_from_slice(&argmax_val.to_le_bytes());
    let name_bytes = name.as_bytes();
    let copy_len = name_bytes.len().min(63);
    hdr[48..48 + copy_len].copy_from_slice(&name_bytes[..copy_len]);
    let summary_count = n.min(100);
    for i in 0..summary_count {
        let off = 112 + i * 4;
        if off + 4 <= SECTOR_SIZE {
            hdr[off..off + 4].copy_from_slice(&data[i].to_le_bytes());
        }
    }

    let _ = libfolk::sys::block::write_sector(DUMP_HEADER_SECTOR, &hdr);

    // Write data sectors (2-257)
    let mut buf = [0u8; SECTOR_SIZE];
    let data_sectors = ((n_dumped as usize * 4) + SECTOR_SIZE - 1) / SECTOR_SIZE;
    let data_sectors = data_sectors.min(DUMP_MAX_SECTORS);
    for s in 0..data_sectors {
        let float_start = s * (SECTOR_SIZE / 4);
        let float_end = ((s + 1) * (SECTOR_SIZE / 4)).min(n_dumped as usize);
        for b in buf.iter_mut() { *b = 0; }
        for i in float_start..float_end {
            let off = (i - float_start) * 4;
            buf[off..off + 4].copy_from_slice(&data[i].to_le_bytes());
        }
        let _ = libfolk::sys::block::write_sector(DUMP_DATA_SECTOR + s as u64, &buf);
    }
}

/// Convenience: dump logits after forward pass
#[allow(dead_code)]
pub fn debug_dump_logits(logits: &[f32], label: &str) {
    debug_dump_tensor(label, logits, logits.len() as u32, 0);
}

/// Convenience: dump a 1D hidden state
#[allow(dead_code)]
pub fn debug_dump_hidden(data: &[f32], label: &str) {
    debug_dump_tensor(label, data, data.len() as u32, 0);
}

/// Write health telemetry to sector 259.
/// Layout: HLTH(4) + gen_step(u32) + token_id(u32) + mse(f32) + threshold(f32)
///       + min_mse(f32) + min_mse_step(u32) + total_steps(u32)
pub fn write_health_sector(
    gen_step: u32, token_id: u32, mse: f32, threshold: f32,
    min_mse: f32, min_mse_step: u32, total_steps: u32,
) {
    let mut buf = [0u8; SECTOR_SIZE];
    buf[0..4].copy_from_slice(b"HLTH");
    buf[4..8].copy_from_slice(&gen_step.to_le_bytes());
    buf[8..12].copy_from_slice(&token_id.to_le_bytes());
    buf[12..16].copy_from_slice(&mse.to_le_bytes());
    buf[16..20].copy_from_slice(&threshold.to_le_bytes());
    // "Check Engine" light: lowest MSE seen this run (worst collapse point)
    buf[20..24].copy_from_slice(&min_mse.to_le_bytes());
    buf[24..28].copy_from_slice(&min_mse_step.to_le_bytes());
    buf[28..32].copy_from_slice(&total_steps.to_le_bytes());
    let _ = libfolk::sys::block::write_sector(HEALTH_SECTOR, &buf);
}
