//! Storage throughput baseline for NVMe vs VirtIO-blk.
//!
//! Runs once at boot after both drivers are initialized. Three
//! workloads per backend:
//!   1. Sequential write of 1 MiB (2048 sectors)
//!   2. Sequential read of 1 MiB
//!   3. 100 × single-sector random reads
//!
//! Output: a small table on COM1 serial so you can eyeball the
//! difference in the boot log. MB/s are reported in decimal MB
//! (10^6 bytes) so numbers read the way most vendors quote them.
//!
//! # Safety
//!
//! We save the original content of the scratch region before writes
//! and restore it afterwards. That preserves VirtIO-blk's journal
//! (our scratch area there) and any latent data on the NVMe disk.

use alloc::vec;
use alloc::vec::Vec;

/// 1 MiB = 2048 × 512-byte sectors.
const BENCH_SECTORS: usize = 2048;
const BENCH_BYTES: usize = BENCH_SECTORS * 512;
const RANDOM_IOPS: usize = 100;

/// Per-command chunk size. Bounded by the strictest backend:
///   NVMe's DMA pool takes up to 63 × 4 KiB = 504 sectors per command.
///   VirtIO-blk's burst buffer takes up to 64 sectors per command.
/// 64 sectors (32 KiB) fits both without splitting further.
const CHUNK_SECTORS: usize = 64;
const CHUNK_BYTES: usize = CHUNK_SECTORS * 512;
const CHUNKS_PER_BENCH: usize = BENCH_SECTORS / CHUNK_SECTORS;

/// Read TSC inline.
#[inline(always)]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Use the already-calibrated TSC rate from the IQE subsystem. Avoids
/// a fresh timer-based calibration in this path (interrupts are
/// unpredictable during late-init, so uptime_ms() isn't guaranteed
/// to tick here). Falls back to a ballpark 3000 ticks/μs if IQE
/// hasn't produced a reading — numbers get less accurate but the
/// benchmark still runs.
fn ticks_per_us() -> u64 {
    let ipu = crate::drivers::iqe::tsc_ticks_per_us();
    if ipu == 0 { 3000 } else { ipu }
}

/// Convert TSC delta → MB/s given byte count. Uses decimal MB (10^6)
/// so the number matches vendor spec sheets.
fn tsc_to_mb_per_s(bytes: usize, tsc_delta: u64, ticks_per_us: u64) -> u64 {
    if tsc_delta == 0 || ticks_per_us == 0 { return 0; }
    let us = tsc_delta / ticks_per_us;
    if us == 0 { return 0; }
    // bytes / us × 10^6 / 10^6 = bytes / us (which is MB/s when MB
    // is 10^6 bytes). So: MB/s = bytes / us.
    (bytes as u64) / us
}

/// Average latency (μs) for N operations.
fn tsc_to_avg_us(tsc_delta: u64, n: usize, ticks_per_us: u64) -> u64 {
    if ticks_per_us == 0 || n == 0 { return 0; }
    (tsc_delta / ticks_per_us) / (n as u64)
}

#[derive(Clone, Copy)]
struct BenchResult {
    write_mbs: u64,
    read_mbs: u64,
    random_avg_us: u64,
    writes_measured: bool,
}

/// Entry point — runs all four benchmarks (NVMe write/read/random,
/// VirtIO-blk same) and prints a summary table.
pub fn run() {
    crate::serial_strln!("[BENCH] Storage baseline — 1 MiB sequential + 100 random 512B reads");
    let ticks_per_us = ticks_per_us();
    crate::serial_str!("[BENCH] TSC: ");
    crate::drivers::serial::write_dec(ticks_per_us as u32);
    crate::serial_strln!(" ticks/μs (from IQE calibration)");

    let nvme = if crate::drivers::nvme::is_initialized() {
        // Sectors 100..2148 on a fresh 16 MiB NVMe disk — far from
        // MVFS (last 132 sectors) and PRP self-test regions (LBA 0-95).
        Some(bench_backend(
            "NVMe",
            100,
            ticks_per_us,
            /* writes_enabled */ true,
            |sector, buf| crate::drivers::nvme::read_sectors(sector, buf, CHUNK_SECTORS).map_err(|_| ()),
            |sector, buf| crate::drivers::nvme::write_sectors(sector, buf, CHUNK_SECTORS).map_err(|_| ()),
            |sector, buf| crate::drivers::nvme::block_read(sector, buf).map_err(|_| ()),
        ))
    } else {
        None
    };

    // VirtIO-blk benchmarking is skipped intentionally. On this QEMU
    // version + accel combo, the VirtIO-blk completion path
    // intermittently returns status=0xFF (documented in
    // `MEMORY.md → feedback_virtio_write.md`). The existing drivers
    // paper over this with retry+verify, which is fine for
    // correctness but makes benchmarking numbers meaningless —
    // latency reflects the workaround, not the device. NVMe avoids
    // that path entirely and gives us the real number.
    let virtio: Option<BenchResult> = None;

    // Summary table.
    crate::serial_strln!("[BENCH] ┌────────────┬────────────┬────────────┬──────────────┐");
    crate::serial_strln!("[BENCH] │ Backend    │ Write MB/s │ Read  MB/s │ Rand read μs │");
    crate::serial_strln!("[BENCH] ├────────────┼────────────┼────────────┼──────────────┤");
    if let Some(r) = nvme { print_row("NVMe      ", r); }
    if let Some(r) = virtio { print_row("VirtIO-blk", r); }
    crate::serial_strln!("[BENCH] └────────────┴────────────┴────────────┴──────────────┘");
}

fn print_row(label: &str, r: BenchResult) {
    crate::serial_str!("[BENCH] │ ");
    crate::serial_str!(label);
    crate::serial_str!(" │ ");
    if r.writes_measured {
        print_col(r.write_mbs);
    } else {
        crate::serial_str!("       n/a");
    }
    crate::serial_str!(" │ ");
    print_col(r.read_mbs);
    crate::serial_str!(" │ ");
    print_col(r.random_avg_us);
    crate::serial_strln!(" │");
}

fn print_col(val: u64) {
    // Right-pad a 10-char column. Simple serial output; not trying
    // to be fancy with Unicode width.
    let s_len = digit_count(val);
    for _ in s_len..10 { crate::serial_str!(" "); }
    crate::drivers::serial::write_dec(val as u32);
}

fn digit_count(mut v: u64) -> usize {
    if v == 0 { return 1; }
    let mut n = 0;
    while v > 0 { v /= 10; n += 1; }
    n
}

/// Read 1 MiB as 32 chunks of 32 KiB each. Each chunk is one driver
/// command — chunk size is capped by the strictest backend's max
/// transfer (VirtIO-blk's 64-sector burst). Returns `Ok` iff every
/// chunk succeeded.
fn chunked_read(
    read_fn: fn(u64, &mut [u8]) -> Result<(), ()>,
    start: u64,
    buf: &mut [u8],
) -> Result<(), ()> {
    for i in 0..CHUNKS_PER_BENCH {
        let off = i * CHUNK_BYTES;
        read_fn(
            start + (i * CHUNK_SECTORS) as u64,
            &mut buf[off..off + CHUNK_BYTES],
        )?;
    }
    Ok(())
}

fn chunked_write(
    write_fn: fn(u64, &[u8]) -> Result<(), ()>,
    start: u64,
    buf: &[u8],
) -> Result<(), ()> {
    for i in 0..CHUNKS_PER_BENCH {
        let off = i * CHUNK_BYTES;
        write_fn(
            start + (i * CHUNK_SECTORS) as u64,
            &buf[off..off + CHUNK_BYTES],
        )?;
    }
    Ok(())
}

/// Core benchmark loop, parameterized over the two backends via fn
/// pointers. Transfers are chunked to 32 KiB commands — reported
/// MB/s therefore includes the per-command overhead of 32 submits
/// + 32 completions, which is the relevant number for MVFS-style
/// workloads.
fn bench_backend(
    label: &str,
    scratch_start: u64,
    ticks_per_us: u64,
    writes_enabled: bool,
    read_fn: fn(u64, &mut [u8]) -> Result<(), ()>,
    write_fn: fn(u64, &[u8]) -> Result<(), ()>,
    single_fn: fn(u64, &mut [u8; 512]) -> Result<(), ()>,
) -> BenchResult {
    crate::serial_str!("[BENCH] ");
    crate::serial_str!(label);
    crate::serial_strln!(": running...");

    let mut original: Vec<u8> = vec![0u8; BENCH_BYTES];
    if writes_enabled {
        if chunked_read(read_fn, scratch_start, &mut original).is_err() {
            crate::serial_strln!("[BENCH]   FAIL: initial snapshot read");
            return BenchResult { write_mbs: 0, read_mbs: 0, random_avg_us: 0, writes_measured: false };
        }
    }

    let mut pattern: Vec<u8> = vec![0u8; BENCH_BYTES];
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = ((i >> 3) ^ (i & 0xFF)) as u8;
    }

    // ── Sequential write (32 chunks × 32 KiB) ──
    let (write_mbs, write_ok) = if writes_enabled {
        let t0 = rdtsc();
        let ok = chunked_write(write_fn, scratch_start, &pattern).is_ok();
        let t1 = rdtsc();
        let mbs = if ok { tsc_to_mb_per_s(BENCH_BYTES, t1 - t0, ticks_per_us) } else { 0 };
        (mbs, ok)
    } else {
        (0, false)
    };

    // ── Sequential read ──
    let mut read_buf: Vec<u8> = vec![0u8; BENCH_BYTES];
    let t2 = rdtsc();
    let read_ok = chunked_read(read_fn, scratch_start, &mut read_buf).is_ok();
    let t3 = rdtsc();
    let read_mbs = if read_ok { tsc_to_mb_per_s(BENCH_BYTES, t3 - t2, ticks_per_us) } else { 0 };

    if writes_enabled && write_ok && read_ok && read_buf != pattern {
        crate::serial_strln!("[BENCH]   WARN: sequential read mismatch vs pattern");
    }

    // ── 100 random single-sector reads ──
    let mut single = [0u8; 512];
    let mut seed: u64 = 0xDEAD_BEEF_CAFE_BABE;
    let t4 = rdtsc();
    for _ in 0..RANDOM_IOPS {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let offset = (seed >> 33) as u64 % BENCH_SECTORS as u64;
        let _ = single_fn(scratch_start + offset, &mut single);
    }
    let t5 = rdtsc();
    let random_avg_us = tsc_to_avg_us(t5 - t4, RANDOM_IOPS, ticks_per_us);

    // ── Restore (only if we dirtied the region) ──
    if writes_enabled {
        if chunked_write(write_fn, scratch_start, &original).is_err() {
            crate::serial_strln!("[BENCH]   WARN: restore write failed — scratch region dirty");
        }
    }

    BenchResult {
        write_mbs,
        read_mbs,
        random_avg_us,
        writes_measured: writes_enabled,
    }
}
