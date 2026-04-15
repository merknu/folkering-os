//! GGUF model loader: reads FOLKDISK header, mmaps the model into a fixed
//! virtual address range, and DMA-reads the bytes from VirtIO disk in 64KB
//! bursts.

use libfolk::println;
use libfolk::sys::block::{block_read, read_sector, SECTOR_SIZE, DATA_START_SECTOR};
use libfolk::sys::memory::{mmap_at, PROT_READ, PROT_WRITE};
use libtensor::gguf::GgufError;

use crate::consts::{MAX_MODEL_SIZE, MODEL_MMAP_BASE};

/// Attempt to load a GGUF model from VirtIO disk.
///
/// Strategy:
/// 1. Read sector 0 (FOLKDISK header) for model_sector/model_size
/// 2. If header has model info, use it directly
/// 3. Otherwise, fall back to scanning for GGUF magic
///
/// ULTRA 35: Mmap size rounded up to 4KB boundary.
///
/// Returns (pointer, size) on success.
pub fn load_model_from_disk() -> Result<(*const u8, usize), &'static str> {
    let mut header_buf = [0u8; SECTOR_SIZE];

    // Read sector 0 of the VirtIO data disk (FOLKDISK header)
    if read_sector(0, &mut header_buf).is_err() {
        return Err("cannot read sector 0");
    }

    // Check FOLKDISK magic
    let has_folkdisk = &header_buf[0..8] == b"FOLKDISK";

    let mut model_start_sector: u64 = 0;
    let mut model_size: usize = 0;

    if has_folkdisk {
        // Parse model_sector from offset 64 and model_size from offset 72
        let ms = u64::from_le_bytes([
            header_buf[64], header_buf[65], header_buf[66], header_buf[67],
            header_buf[68], header_buf[69], header_buf[70], header_buf[71],
        ]);
        let mz = u64::from_le_bytes([
            header_buf[72], header_buf[73], header_buf[74], header_buf[75],
            header_buf[76], header_buf[77], header_buf[78], header_buf[79],
        ]);

        if ms > 0 && mz > 0 {
            model_start_sector = ms;
            model_size = mz as usize;
            println!("[INFERENCE] FOLKDISK header: model @ sector {}, {} bytes ({} KB)",
                model_start_sector, model_size, model_size / 1024);
        }
    }

    // Fallback: scan first 64 sectors for GGUF magic
    if model_start_sector == 0 {
        println!("[INFERENCE] No model in header, scanning for GGUF magic...");
        let gguf_magic = [0x47u8, 0x55, 0x46, 0x47]; // "GGUF" in LE

        for sector in 0..64u64 {
            let mut scan_buf = [0u8; SECTOR_SIZE];
            if read_sector(DATA_START_SECTOR + sector, &mut scan_buf).is_err() {
                continue;
            }
            if scan_buf[0..4] == gguf_magic {
                model_start_sector = DATA_START_SECTOR + sector;
                // Unknown size — will read until zeros
                break;
            }
        }

        if model_start_sector == 0 {
            return Err("no GGUF magic found");
        }
    }

    // Determine mmap size
    // ULTRA 35: Round up to 4KB boundary
    let mmap_size = if model_size > 0 {
        (model_size + 4095) & !4095 // page-align
    } else {
        MAX_MODEL_SIZE // unknown size, allocate max as fallback
    };

    // Allocate mmap region in chunks (kernel limits mmap to 16MB per call)
    const MMAP_CHUNK: usize = 16 * 1024 * 1024; // 16MB per mmap call
    let n_chunks = (mmap_size + MMAP_CHUNK - 1) / MMAP_CHUNK;
    println!("[INFERENCE] Allocating {}MB in {} chunks of 16MB...", mmap_size / (1024 * 1024), n_chunks);
    let mut mapped = 0usize;
    let mut chunk_idx = 0usize;
    while mapped < mmap_size {
        let chunk = (mmap_size - mapped).min(MMAP_CHUNK);
        let addr = MODEL_MMAP_BASE + mapped;
        println!("[INFERENCE]   mmap chunk {}/{}: addr=0x{:X} size={}MB",
            chunk_idx + 1, n_chunks, addr, chunk / (1024 * 1024));
        if mmap_at(addr, chunk, PROT_READ | PROT_WRITE).is_err() {
            println!("[INFERENCE] *** mmap FAILED at chunk {} (offset {}MB) ***", chunk_idx, mapped / (1024 * 1024));
            return Err("mmap failed");
        }
        mapped += chunk;
        chunk_idx += 1;
    }
    let model_ptr = MODEL_MMAP_BASE as *mut u8;
    println!("[INFERENCE] All {} mmap chunks allocated OK", chunk_idx);

    // Read model data from disk
    let sectors_to_read = if model_size > 0 {
        (model_size + SECTOR_SIZE - 1) / SECTOR_SIZE
    } else {
        MAX_MODEL_SIZE / SECTOR_SIZE
    };

    let mut total_read = 0usize;
    let mut sector = model_start_sector;

    // DMA: 128-sector bursts (64KB per VirtIO request), no yielding
    let burst_sectors = 128usize;
    let total_sectors = sectors_to_read;
    let mut last_progress_mb = 0usize;
    let mut remaining = total_sectors;
    println!("[INFERENCE] Reading {} sectors ({} MB) via {} DMA bursts (256KB each)...",
        total_sectors, model_size / (1024 * 1024), (total_sectors + burst_sectors - 1) / burst_sectors);

    while remaining > 0 {
        let n = remaining.min(burst_sectors);
        let buf = unsafe {
            core::slice::from_raw_parts_mut(model_ptr.add(total_read), n * SECTOR_SIZE)
        };

        match block_read(sector, buf, n) {
            Ok(()) => {
                total_read += n * SECTOR_SIZE;
                sector += n as u64;
                remaining -= n;

                // Progress logging every 32MB
                let current_mb = total_read / (1024 * 1024);
                if current_mb >= last_progress_mb + 32 {
                    println!("[INFERENCE] Loaded {}MB / {}MB",
                        current_mb, model_size / (1024 * 1024));
                    last_progress_mb = current_mb;
                }

                // If we don't know model_size, check for zero sectors
                if model_size == 0 && total_read > SECTOR_SIZE * 2 {
                    let last = &buf[(n - 1) * SECTOR_SIZE..n * SECTOR_SIZE];
                    if last.iter().all(|&b| b == 0) {
                        total_read -= SECTOR_SIZE;
                        break;
                    }
                }
            }
            Err(_) => {
                println!("[INFERENCE] *** DMA read FAILED at sector {}, {}MB read so far ***", sector, total_read / (1024*1024));
                break;
            }
        }
    }

    if total_read == 0 {
        return Err("no data read");
    }

    // Use exact model_size if known, otherwise use total_read
    let final_size = if model_size > 0 { model_size } else { total_read };

    // Debug: check first 16 bytes of loaded data
    let first_bytes = unsafe { core::slice::from_raw_parts(model_ptr, 16.min(final_size)) };
    println!("[INFERENCE] First bytes: {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X} {:02X}",
        first_bytes[0], first_bytes[1], first_bytes[2], first_bytes[3],
        first_bytes[4], first_bytes[5], first_bytes[6], first_bytes[7]);

    Ok((model_ptr as *const u8, final_size))
}

pub fn gguf_error_str(e: GgufError) -> &'static str {
    match e {
        GgufError::InvalidMagic => "invalid magic",
        GgufError::UnsupportedVersion(_) => "unsupported version",
        GgufError::TruncatedData => "truncated data",
        GgufError::InvalidMetadata => "invalid metadata",
        GgufError::InvalidTensor => "invalid tensor",
    }
}
