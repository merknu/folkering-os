//! Mutable VFS (tmpfs) — volatile, in-kernel file store.
//!
//! Gives userspace a write-path that doesn't require the Synapse
//! daemon or raw block I/O. Files live in kernel heap (`Vec<u8>`
//! per entry), so contents disappear on reboot. Phase 2 will back
//! this with disk sectors for persistence.
//!
//! Bounded to keep memory use predictable:
//!   MVFS_MAX_FILES × MVFS_MAX_FILE_SIZE = 16 × 4 KiB = 64 KiB max
//!
//! Separate from the read-only ramdisk (`fs::ramdisk`): this module
//! doesn't replace any existing FS semantics, it adds a parallel
//! namespace reachable via its own syscalls.
//!
//! Thread-safety: single global Mutex-protected Vec. All mutations
//! take the lock; lookups do too. Fine for the current IPC-serialized
//! workload.

use alloc::vec::Vec;
use spin::Mutex;
use lazy_static::lazy_static;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

// ── Persistence backend dispatcher ─────────────────────────────────────
//
// MVFS started life on VirtIO-blk. NVMe arrived later and is now the
// preferred backend when present. Rather than sprinkle `if nvme_ready
// { nvme::... } else { virtio_blk::... }` through every call site, we
// route through a single enum + static selector. Swapping backends at
// runtime is just a store to `ACTIVE_BACKEND`.
//
// Error types from the two drivers differ, so dispatchers return
// `Result<(), ()>`: the caller only needs the boolean "did it work".

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    VirtioBlk = 0,
    Nvme = 1,
}

static ACTIVE_BACKEND: AtomicU8 = AtomicU8::new(Backend::VirtioBlk as u8);

fn active_backend() -> Backend {
    match ACTIVE_BACKEND.load(Ordering::Acquire) {
        1 => Backend::Nvme,
        _ => Backend::VirtioBlk,
    }
}

/// Switch MVFS persistence to the NVMe controller. Callers should
/// only flip this once NVMe is confirmed ready and (ideally) before
/// the first `load_from_disk` of the boot.
pub fn use_nvme_backend() {
    ACTIVE_BACKEND.store(Backend::Nvme as u8, Ordering::Release);
    crate::serial_strln!("[MVFS] backend switched to NVMe");
}

/// Revert MVFS to VirtIO-blk. Exported for tests and emergency
/// recovery — there's no production flow that flips back.
pub fn use_virtio_backend() {
    ACTIVE_BACKEND.store(Backend::VirtioBlk as u8, Ordering::Release);
}

fn be_is_initialized() -> bool {
    match active_backend() {
        Backend::VirtioBlk => crate::drivers::virtio_blk::is_initialized(),
        Backend::Nvme => crate::drivers::nvme::is_initialized(),
    }
}

fn be_capacity() -> u64 {
    match active_backend() {
        Backend::VirtioBlk => crate::drivers::virtio_blk::capacity(),
        Backend::Nvme => crate::drivers::nvme::capacity_sectors(),
    }
}

fn be_block_read(sector: u64, buf: &mut [u8; SECTOR]) -> Result<(), ()> {
    match active_backend() {
        Backend::VirtioBlk => crate::drivers::virtio_blk::block_read(sector, buf).map_err(|_| ()),
        Backend::Nvme => crate::drivers::nvme::block_read(sector, buf).map_err(|_| ()),
    }
}

fn be_block_write(sector: u64, buf: &[u8; SECTOR]) -> Result<(), ()> {
    match active_backend() {
        Backend::VirtioBlk => crate::drivers::virtio_blk::block_write(sector, buf).map_err(|_| ()),
        Backend::Nvme => crate::drivers::nvme::block_write(sector, buf).map_err(|_| ()),
    }
}

fn be_read_sectors(start: u64, buf: &mut [u8], count: usize) -> Result<(), ()> {
    match active_backend() {
        Backend::VirtioBlk => crate::drivers::virtio_blk::read_sectors(start, buf, count).map_err(|_| ()),
        Backend::Nvme => crate::drivers::nvme::read_sectors(start, buf, count).map_err(|_| ()),
    }
}

fn be_write_sectors(start: u64, buf: &[u8], count: usize) -> Result<(), ()> {
    match active_backend() {
        Backend::VirtioBlk => crate::drivers::virtio_blk::write_sectors(start, buf, count).map_err(|_| ()),
        Backend::Nvme => crate::drivers::nvme::write_sectors(start, buf, count).map_err(|_| ()),
    }
}

pub const MVFS_MAX_FILES: usize = 16;
pub const MVFS_MAX_NAME: usize = 32;
pub const MVFS_MAX_FILE_SIZE: usize = 4096;

// ── On-disk layout (Phase 2 persistence) ──────────────────────────────
//
//   sector 0:      magic + version + entry_count
//   sectors 1-2:   entry table (16 × 64 B)
//   sector 3:      reserved (future: slot-usage bitmap, journal ptr)
//   sectors 4-131: file data, 8 sectors (4 KiB) per slot, slot `i`
//                  at sectors (4 + i*8) through (4 + i*8 + 7)
//
// Region lives at the very end of the VirtIO data disk so folk-pack
// doesn't need to reserve it explicitly — the tail bytes are already
// zero-initialized after model pack, which deserializes as an empty
// MVFS (magic mismatch → ignore, start fresh).

const MVFS_MAGIC: [u8; 8] = *b"FOLKMVFS";
const MVFS_VERSION: u16 = 1;
/// Total sectors consumed by the MVFS region on disk (66 KiB).
const MVFS_SECTORS: u64 = 132;
/// Sectors per data slot. 8 × 512 = 4096 matches `MVFS_MAX_FILE_SIZE`.
const MVFS_SLOT_SECTORS: u64 = 8;
/// Byte size of each entry-table record. 16 × 64 = 1024 B = 2 sectors.
const MVFS_TABLE_RECORD: usize = 64;

const SECTOR: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MvfsError {
    /// Name too long or empty.
    InvalidName,
    /// File contents exceed `MVFS_MAX_FILE_SIZE`.
    DataTooLarge,
    /// File doesn't exist.
    NotFound,
    /// Table full (`MVFS_MAX_FILES` entries already in use) and the
    /// name doesn't match an existing entry.
    TableFull,
}

struct MvfsEntry {
    name_len: u8,
    name: [u8; MVFS_MAX_NAME],
    data: Vec<u8>,
}

impl MvfsEntry {
    fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
}

lazy_static! {
    static ref MVFS: Mutex<Vec<MvfsEntry>> = Mutex::new(Vec::new());
    /// Serialize disk-flush operations. Without this, two concurrent
    /// writers can each snapshot in-memory state, drop the main MVFS
    /// lock, and race on the 132-sector disk region. Each task's
    /// ~132 sector writes interleave arbitrarily, so the on-disk
    /// state can mix one task's header with another's table/data —
    /// a torn flush that `load_from_disk` happily deserializes into
    /// a corrupt entry set.
    ///
    /// Taken only by `flush_to_disk`; `read`/`write`/`delete`
    /// in-memory ops never touch it. Reads therefore don't block on
    /// in-flight I/O.
    static ref MVFS_FLUSH: Mutex<()> = Mutex::new(());
}

// ── Dirty-slot tracking for partial flush ────────────────────────────
//
// Bit `i` = slot `i` needs its 8 data sectors rewritten.
// Bit 31  = header + entry table need to be rewritten.
//
// Initially set to "all dirty" so the first flush after boot writes
// the full region (treating whatever was on disk as opaque). Every
// subsequent write only marks its own slot + the header, so steady-
// state flushes touch ~10 sectors instead of 132 — an ~8x reduction
// in VirtIO traffic per MVFS mutation.
//
// Delete marks all trailing slots dirty because Vec::retain shifts
// indices down, so disk slots [i..old_len) no longer match memory.
const DIRTY_HEADER: u32 = 1 << 31;
/// Compile-time guard: slot bits (0..MVFS_MAX_FILES) and DIRTY_HEADER
/// (bit 31) must not overlap. If someone bumps MVFS_MAX_FILES past 31,
/// the slot mask would eat into DIRTY_HEADER — this static-assert
/// turns that into a build error instead of silent torn flushes.
const _: () = assert!(MVFS_MAX_FILES < 31, "MVFS_MAX_FILES would collide with DIRTY_HEADER bit");

/// Initial value: nothing dirty. `load_from_disk` leaves the table
/// in sync with disk, and `flush_to_disk` is a no-op when dirty==0.
/// Any write/delete sets specific bits before calling flush.
static DIRTY: AtomicU32 = AtomicU32::new(0);

#[inline]
fn mark_dirty(bits: u32) {
    DIRTY.fetch_or(bits, Ordering::Relaxed);
}

#[inline]
fn take_dirty() -> u32 {
    // Atomically read and clear. Concurrent writes during flush are
    // impossible because MVFS_FLUSH serializes — but the marker ops
    // from write/delete don't take MVFS_FLUSH, so we still need an
    // atomic swap here to avoid losing bits set after our snapshot.
    DIRTY.swap(0, Ordering::AcqRel)
}

fn validate_name(name: &[u8]) -> Result<u8, MvfsError> {
    if name.is_empty() || name.len() > MVFS_MAX_NAME {
        return Err(MvfsError::InvalidName);
    }
    Ok(name.len() as u8)
}

/// Write (create or overwrite) a file with the given name.
pub fn write(name: &[u8], data: &[u8]) -> Result<(), MvfsError> {
    let name_len = validate_name(name)?;
    if data.len() > MVFS_MAX_FILE_SIZE {
        return Err(MvfsError::DataTooLarge);
    }

    let dirty_slot = {
        let mut table = MVFS.lock();

        // Overwrite path: look for matching name.
        let mut found_idx: Option<usize> = None;
        for (i, entry) in table.iter_mut().enumerate() {
            if entry.name_bytes() == name {
                entry.data.clear();
                entry.data.extend_from_slice(data);
                found_idx = Some(i);
                break;
            }
        }

        if let Some(i) = found_idx {
            i
        } else {
            // Insert path: need a free slot.
            if table.len() >= MVFS_MAX_FILES {
                return Err(MvfsError::TableFull);
            }
            let mut name_buf = [0u8; MVFS_MAX_NAME];
            name_buf[..name.len()].copy_from_slice(name);
            let mut data_vec = Vec::with_capacity(data.len());
            data_vec.extend_from_slice(data);
            let idx = table.len();
            table.push(MvfsEntry {
                name_len,
                name: name_buf,
                data: data_vec,
            });
            idx
        }
    }; // drop table lock before flush — flush_to_disk re-acquires it

    // Mark only the mutated slot + header as dirty. Partial flush
    // rewrites ~10 sectors instead of the full 132.
    mark_dirty((1u32 << dirty_slot) | DIRTY_HEADER);

    flush_to_disk();
    Ok(())
}

/// Read a file into `out`, returning bytes copied. `NotFound` if the
/// name doesn't exist.
pub fn read(name: &[u8], out: &mut [u8]) -> Result<usize, MvfsError> {
    validate_name(name)?;
    let table = MVFS.lock();
    for entry in table.iter() {
        if entry.name_bytes() == name {
            let n = entry.data.len().min(out.len());
            out[..n].copy_from_slice(&entry.data[..n]);
            return Ok(n);
        }
    }
    Err(MvfsError::NotFound)
}

/// Delete a file by name. Returns `true` if removed, `false` if the
/// name wasn't found.
pub fn delete(name: &[u8]) -> bool {
    if validate_name(name).is_err() {
        return false;
    }
    let dirty_mask = {
        let mut table = MVFS.lock();
        // Find the victim index so we know which trailing slots
        // will shift. Everything from that index onwards is dirty
        // because Vec::retain shifts later entries down by one —
        // on-disk slot positions no longer match memory positions.
        let victim_idx = table.iter().position(|e| e.name_bytes() == name);
        let old_len = table.len();
        match victim_idx {
            Some(i) => {
                table.remove(i);
                // Slots [i..old_len) are all dirty. Build a mask
                // covering exactly those bits, plus the header.
                let mask_lo = (1u32 << i) - 1;
                let mask_hi = if old_len >= 32 { u32::MAX } else { (1u32 << old_len) - 1 };
                Some((mask_hi & !mask_lo) | DIRTY_HEADER)
            }
            None => None,
        }
    };

    match dirty_mask {
        Some(bits) => {
            mark_dirty(bits);
            flush_to_disk();
            true
        }
        None => false,
    }
}

/// Serialize the directory listing into `out` as a flat byte stream
/// of `[name_len: u8][name bytes]` pairs. Returns bytes written.
/// Stops early if `out` fills up — partial listings are OK.
///
/// If `prefix` is non-empty, only entries whose name starts with the
/// prefix are included. Callers use this to simulate subdirectories
/// — e.g. prefix `"logs/"` lists every entry under that pseudo-dir.
/// Pass `&[]` to list every entry.
pub fn list(prefix: &[u8], out: &mut [u8]) -> usize {
    let table = MVFS.lock();
    let mut pos = 0usize;
    for entry in table.iter() {
        let name = entry.name_bytes();
        if !prefix.is_empty() && !name.starts_with(prefix) {
            continue;
        }
        let nl = name.len();
        let needed = 1 + nl;
        if pos + needed > out.len() {
            break;
        }
        out[pos] = entry.name_len;
        out[pos + 1..pos + 1 + nl].copy_from_slice(name);
        pos += needed;
    }
    pos
}

// ── Disk persistence (Phase 2) ────────────────────────────────────────

/// Compute the first sector of the MVFS region on the VirtIO disk.
/// Returns `None` if the block device isn't ready yet or the disk is
/// too small to hold a full MVFS region.
fn mvfs_start() -> Option<u64> {
    if !be_is_initialized() {
        return None;
    }
    let cap = be_capacity();
    if cap < MVFS_SECTORS {
        return None;
    }
    Some(cap - MVFS_SECTORS)
}

/// Attempt to load MVFS state from disk on boot. No-op if:
///   - block device unavailable
///   - region magic doesn't match (fresh disk, first boot ever)
///   - header/table reads fail for any reason
///
/// In all error paths the in-memory Vec stays empty — Phase 1
/// semantics continue to hold.
pub fn load_from_disk() {
    let start = match mvfs_start() {
        Some(s) => s,
        None => return,
    };

    // ── Read header sector.
    let mut header = [0u8; SECTOR];
    if be_block_read(start, &mut header).is_err() {
        return;
    }
    if header[0..8] != MVFS_MAGIC {
        // Pristine or unrelated tail data — fresh MVFS.
        return;
    }
    let version = u16::from_le_bytes([header[8], header[9]]);
    if version != MVFS_VERSION {
        crate::serial_strln!("[MVFS] skipping load: unsupported on-disk version");
        return;
    }
    let entry_count = u16::from_le_bytes([header[10], header[11]]) as usize;
    if entry_count > MVFS_MAX_FILES {
        crate::serial_strln!("[MVFS] skipping load: entry_count exceeds max");
        return;
    }

    // ── Read entry table (2 sectors).
    let mut table_bytes = [0u8; SECTOR * 2];
    if be_read_sectors(start + 1, &mut table_bytes, 2).is_err() {
        return;
    }

    // ── Read each in-use data slot, build in-memory entries.
    let mut new_entries: Vec<MvfsEntry> = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let base = i * MVFS_TABLE_RECORD;
        let name_len = table_bytes[base];
        if name_len == 0 || name_len as usize > MVFS_MAX_NAME {
            crate::serial_strln!("[MVFS] skipping load: malformed name_len");
            return;
        }
        let mut name = [0u8; MVFS_MAX_NAME];
        name.copy_from_slice(&table_bytes[base + 1..base + 1 + MVFS_MAX_NAME]);
        let size = u32::from_le_bytes([
            table_bytes[base + 33],
            table_bytes[base + 34],
            table_bytes[base + 35],
            table_bytes[base + 36],
        ]) as usize;
        if size > MVFS_MAX_FILE_SIZE {
            crate::serial_strln!("[MVFS] skipping load: size exceeds max");
            return;
        }

        // Data lives at sectors (start + 4 + i*8) through +7.
        let mut slot_buf = [0u8; SECTOR * MVFS_SLOT_SECTORS as usize];
        let slot_start = start + 4 + (i as u64) * MVFS_SLOT_SECTORS;
        if be_read_sectors(
            slot_start,
            &mut slot_buf,
            MVFS_SLOT_SECTORS as usize,
        )
        .is_err()
        {
            return;
        }

        let mut data = Vec::with_capacity(size);
        data.extend_from_slice(&slot_buf[..size]);
        new_entries.push(MvfsEntry { name_len, name, data });
    }

    // ── Swap in atomically.
    {
        let mut table = MVFS.lock();
        *table = new_entries;
    }
    crate::serial_str!("[MVFS] loaded ");
    crate::drivers::serial::write_dec(entry_count as u32);
    crate::serial_strln!(" entries from disk");
}

/// Write the current MVFS state to disk as a full region rewrite.
/// Called from `write` and `delete` after the in-memory state has
/// been updated.
///
/// Silent on success to avoid log spam; logs a single line on any
/// sector I/O failure.
fn flush_to_disk() {
    let start = match mvfs_start() {
        Some(s) => s,
        None => return,
    };

    // Serialize the I/O portion so concurrent writers can't interleave
    // sector writes and leave the 132-sector region in a torn state.
    // See `MVFS_FLUSH` lazy_static doc for rationale.
    let _flush_guard = MVFS_FLUSH.lock();

    // Atomically claim every dirty bit set since the last flush. Bits
    // set AFTER this point by concurrent mutators go into the next
    // flush — correct because those bits reflect state the current
    // snapshot won't observe anyway.
    let dirty = take_dirty();
    if dirty == 0 {
        return; // nothing to do
    }

    // Snapshot the in-memory state under the main MVFS lock, then
    // drop that lock so reads from other tasks aren't blocked by the
    // ~tens-of-ms of disk I/O that follows. We hold `MVFS_FLUSH` for
    // the entire flush so a later writer's flush will queue behind
    // this one, but its in-memory mutation already completed — the
    // serialization is strictly about the disk-side commit order.
    let snapshot: Vec<(u8, [u8; MVFS_MAX_NAME], Vec<u8>)> = {
        let table = MVFS.lock();
        table
            .iter()
            .map(|e| (e.name_len, e.name, e.data.clone()))
            .collect()
    };

    // Error-recovery invariant: if ANY sector write fails during
    // this flush, re-mark the ENTIRE original `dirty` mask so the
    // next flush retries from scratch. Partial restoration (e.g.
    // "header ok, don't redo it") is unsafe — if header succeeded
    // but the table write failed, on-disk state is torn: the header
    // claims a new entry_count while the table still has the old
    // layout. A future `load_from_disk` would trust the header and
    // deserialize garbage from the stale table. Full re-retry is
    // the only correct recovery path here.
    //
    // Write order (data → table → header) would be safer — header
    // is the "commit point" if written last — but we keep the
    // current order for simplicity and rely on full retry.
    if dirty & DIRTY_HEADER != 0 {
        let mut header = [0u8; SECTOR];
        header[0..8].copy_from_slice(&MVFS_MAGIC);
        header[8..10].copy_from_slice(&MVFS_VERSION.to_le_bytes());
        header[10..12].copy_from_slice(&(snapshot.len() as u16).to_le_bytes());
        if be_block_write(start, &header).is_err() {
            crate::serial_strln!("[MVFS] flush: header write failed");
            mark_dirty(dirty);
            return;
        }

        let mut table_bytes = [0u8; SECTOR * 2];
        for (i, (name_len, name, data)) in snapshot.iter().enumerate() {
            let base = i * MVFS_TABLE_RECORD;
            table_bytes[base] = *name_len;
            table_bytes[base + 1..base + 1 + MVFS_MAX_NAME].copy_from_slice(name);
            let size = data.len() as u32;
            table_bytes[base + 33..base + 37].copy_from_slice(&size.to_le_bytes());
        }
        if be_write_sectors(start + 1, &table_bytes, 2).is_err() {
            crate::serial_strln!("[MVFS] flush: table write failed");
            mark_dirty(dirty);
            return;
        }
    }

    // ── Data slots: only write the ones whose dirty bit is set AND
    //    whose slot index < current entry count. Slots past the
    //    in-memory count are stale but load_from_disk ignores them
    //    (capped by entry_count), so leaving their bytes on disk is
    //    harmless — saves 8 sectors per deleted-but-unused slot.
    for (i, (_, _, data)) in snapshot.iter().enumerate() {
        if dirty & (1u32 << i) == 0 { continue; }
        let mut slot_buf = [0u8; SECTOR * MVFS_SLOT_SECTORS as usize];
        slot_buf[..data.len()].copy_from_slice(data);
        let slot_start = start + 4 + (i as u64) * MVFS_SLOT_SECTORS;
        if be_write_sectors(
            slot_start,
            &slot_buf,
            MVFS_SLOT_SECTORS as usize,
        )
        .is_err()
        {
            crate::serial_str!("[MVFS] flush: slot ");
            crate::drivers::serial::write_dec(i as u32);
            crate::serial_strln!(" write failed");
            // Restore ALL remaining bits (including already-written
            // slots' bits): while those slots' on-disk data is
            // technically good, rewriting a few is cheaper than
            // tracking exactly which ones made it and having the
            // retry logic cope with partial states.
            mark_dirty(dirty);
            return;
        }
    }
}
