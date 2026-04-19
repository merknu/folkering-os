//! Filesystem syscalls: ramdisk read_dir/read_file + raw block I/O.

// ── Block Device ───────────────────────────────────────────────────────

pub fn syscall_block_read(sector: u64, buf_ptr: u64, count: u64) -> u64 {
    use crate::drivers::virtio_blk;

    if !virtio_blk::is_initialized() {
        return u64::MAX;
    }

    // Capability gate: raw block I/O exposes the entire disk —
    // Synapse's SQLite region, the MVFS tail region, the GGUF model,
    // and the FOLKDISK boot header. Without this sentinel, any task
    // could read out the whole on-disk state. Granted only to the
    // synapse task at boot; every other task must go through synapse
    // IPC or the MVFS syscalls.
    let task_id = crate::task::task::get_current_task();
    if !crate::capability::has_raw_block_io(task_id) {
        return u64::MAX;
    }

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX;
    }

    // Whole write range must stay in userspace (under the canonical
    // boundary). Previously only `buf_ptr` itself was checked, so a
    // caller near the boundary could straddle into noncanonical /
    // kernel space and corrupt memory on the tail sectors.
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    let total_bytes = count.saturating_mul(512);
    let buf_end = match buf_ptr.checked_add(total_bytes) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if buf_ptr >= USERSPACE_TOP || buf_end > USERSPACE_TOP {
        return u64::MAX;
    }

    let count = count as usize;
    let mut offset = 0usize;
    let mut sec = sector;
    let mut remaining = count;

    while remaining > 0 {
        let burst = remaining.min(virtio_blk::MAX_BURST_SECTORS);
        let data_len = burst * 512;

        if burst > 1 {
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    (buf_ptr as usize + offset) as *mut u8,
                    data_len,
                )
            };

            match virtio_blk::block_read_multi(sec, dst, burst) {
                Ok(()) => {
                    offset += data_len;
                    sec += burst as u64;
                    remaining -= burst;
                }
                Err(_) => return u64::MAX,
            }
        } else {
            let mut sector_buf = [0u8; 512];
            match virtio_blk::block_read(sec, &mut sector_buf) {
                Ok(()) => {
                    let dst = (buf_ptr as usize + offset) as *mut u8;
                    unsafe {
                        core::ptr::copy_nonoverlapping(sector_buf.as_ptr(), dst, 512);
                    }
                    offset += 512;
                    sec += 1;
                    remaining -= 1;
                }
                Err(_) => return u64::MAX,
            }
        }
    }
    0
}

pub fn syscall_block_write(sector: u64, buf_ptr: u64, count: u64) -> u64 {
    use crate::drivers::virtio_blk;

    if !virtio_blk::is_initialized() {
        return u64::MAX;
    }

    // Capability gate: see `syscall_block_read`. Write is strictly
    // more dangerous — an unauthorized task could rewrite the GGUF
    // model payload to inject AI-side rootkits, corrupt Synapse's
    // SQLite, or overwrite the FOLKDISK boot header to redirect the
    // entire OS. Synapse-only by design.
    let task_id = crate::task::task::get_current_task();
    if !crate::capability::has_raw_block_io(task_id) {
        return u64::MAX;
    }

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX;
    }

    let buf_len = (count as usize) * virtio_blk::SECTOR_SIZE;

    // Whole read range must stay in userspace. `buf_ptr` alone was
    // checked before; a high `count` near the boundary would still
    // let the syscall read tail bytes from kernel memory onto disk.
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    let buf_end = match buf_ptr.checked_add(buf_len as u64) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if buf_ptr >= USERSPACE_TOP || buf_end > USERSPACE_TOP {
        return u64::MAX;
    }

    let current_task = crate::task::task::get_current_task();
    let _ = virtio_blk::write_journal_entry(current_task, 1, sector, count);

    let buf = unsafe {
        core::slice::from_raw_parts(buf_ptr as *const u8, buf_len)
    };

    match virtio_blk::write_sectors(sector, buf, count as usize) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

// ── Ramdisk ────────────────────────────────────────────────────────────

pub fn syscall_fs_read_dir(buf_ptr: u64, buf_size: u64) -> u64 {
    use crate::fs::format::DirEntry;

    if buf_ptr == 0 || buf_size == 0 || buf_size > 64 * 1024 {
        return u64::MAX;
    }
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    let buf_end = match buf_ptr.checked_add(buf_size) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if buf_ptr < 0x200000 || buf_ptr >= USERSPACE_TOP || buf_end > USERSPACE_TOP {
        return u64::MAX;
    }

    let rd = match crate::fs::ramdisk() {
        Some(rd) => rd,
        None => return 0,
    };

    let entry_size = core::mem::size_of::<DirEntry>();
    let max_entries = buf_size as usize / entry_size;
    let entries = rd.entries();
    let count = entries.len().min(max_entries);

    for i in 0..count {
        let fpk = &entries[i];

        // CRITICAL: Use volatile reads to prevent LLVM from generating SSE instructions
        // that may cause GPF due to alignment assumptions in syscall context.
        let fpk_ptr = fpk as *const _ as *const u8;

        let id = unsafe { core::ptr::read_volatile(fpk_ptr as *const u16) };
        let entry_type = unsafe { core::ptr::read_volatile(fpk_ptr.add(2) as *const u16) };

        let mut name = [0u8; 32];
        for j in 0..32 {
            name[j] = unsafe { core::ptr::read_volatile(fpk_ptr.add(4 + j)) };
        }

        let size = unsafe { core::ptr::read_volatile(fpk_ptr.add(48) as *const u64) };

        let dir_entry = DirEntry {
            id,
            entry_type,
            name,
            size,
        };

        let dst = (buf_ptr as *mut u8).wrapping_add(i * entry_size);
        unsafe {
            let src = &dir_entry as *const DirEntry as *const u8;
            core::ptr::copy_nonoverlapping(src, dst, entry_size);
        }
    }

    count as u64
}

pub fn syscall_fs_read_file(name_ptr: u64, buf_ptr: u64, buf_size: u64) -> u64 {
    if name_ptr == 0 || buf_ptr == 0 || buf_size == 0 || buf_size > 4 * 1024 * 1024 {
        return u64::MAX;
    }
    // Both pointers must land in userspace. `name_ptr` is walked up to
    // 32 bytes for the null terminator; `buf_ptr` can span up to 4 MiB.
    const USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
    let name_end = match name_ptr.checked_add(32) {
        Some(e) => e,
        None => return u64::MAX,
    };
    let buf_end = match buf_ptr.checked_add(buf_size) {
        Some(e) => e,
        None => return u64::MAX,
    };
    if name_ptr < 0x200000 || name_ptr >= USERSPACE_TOP || name_end > USERSPACE_TOP {
        return u64::MAX;
    }
    if buf_ptr < 0x200000 || buf_ptr >= USERSPACE_TOP || buf_end > USERSPACE_TOP {
        return u64::MAX;
    }

    let mut name_buf = [0u8; 32];
    let name_src = name_ptr as *const u8;
    let mut name_len = 0;
    for i in 0..32 {
        let b = unsafe { core::ptr::read(name_src.add(i)) };
        if b == 0 { break; }
        name_buf[i] = b;
        name_len = i + 1;
    }

    let name = match core::str::from_utf8(&name_buf[..name_len]) {
        Ok(s) => s,
        Err(_) => return u64::MAX,
    };

    let rd = match crate::fs::ramdisk() {
        Some(rd) => rd,
        None => return u64::MAX,
    };

    let entry = match rd.find(name) {
        Some(e) => e,
        None => return u64::MAX,
    };

    let data = rd.read(entry);
    let copy_len = data.len().min(buf_size as usize);

    unsafe {
        core::ptr::copy_nonoverlapping(
            data.as_ptr(),
            buf_ptr as *mut u8,
            copy_len,
        );
    }

    copy_len as u64
}

// ── Mutable VFS (tmpfs) ────────────────────────────────────────────────
//
// Write-capable complement to the read-only ramdisk. See
// `kernel/src/fs/mvfs.rs` for semantics. All user pointers are bounded
// to the lower-half userspace window — standard guard so a hostile
// vaddr can't corrupt kernel memory via the ring-0 syscall handler.

const MVFS_USERSPACE_TOP: u64 = 0x0000_8000_0000_0000;
const MVFS_USERSPACE_MIN: u64 = 0x200000;
/// Status code returned on any MVFS failure. Picked to match the
/// existing `u64::MAX` convention used by the other FS syscalls so
/// libfolk can treat it as a single "any error" sentinel.
const MVFS_ERR: u64 = u64::MAX;

/// Validate a `[ptr, ptr + len)` range lies fully in userspace.
#[inline]
fn mvfs_range_ok(ptr: u64, len: u64) -> bool {
    if ptr < MVFS_USERSPACE_MIN || ptr >= MVFS_USERSPACE_TOP {
        return false;
    }
    match ptr.checked_add(len) {
        Some(end) => end <= MVFS_USERSPACE_TOP,
        None => false,
    }
}

pub fn syscall_mvfs_write(name_ptr: u64, name_len: u64, data_ptr: u64, data_len: u64) -> u64 {
    if name_len == 0 || name_len > crate::fs::mvfs::MVFS_MAX_NAME as u64 { return MVFS_ERR; }
    if data_len > crate::fs::mvfs::MVFS_MAX_FILE_SIZE as u64 { return MVFS_ERR; }
    if !mvfs_range_ok(name_ptr, name_len) { return MVFS_ERR; }
    // Empty file (len=0) is legitimate — used to create a marker.
    if data_len > 0 && !mvfs_range_ok(data_ptr, data_len) { return MVFS_ERR; }

    let name = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let data = if data_len == 0 {
        &[] as &[u8]
    } else {
        unsafe { core::slice::from_raw_parts(data_ptr as *const u8, data_len as usize) }
    };

    match crate::fs::mvfs::write(name, data) {
        Ok(()) => 0,
        Err(_) => MVFS_ERR,
    }
}

pub fn syscall_mvfs_read(name_ptr: u64, name_len: u64, buf_ptr: u64, buf_max: u64) -> u64 {
    if name_len == 0 || name_len > crate::fs::mvfs::MVFS_MAX_NAME as u64 { return MVFS_ERR; }
    if buf_max == 0 || buf_max > crate::fs::mvfs::MVFS_MAX_FILE_SIZE as u64 { return MVFS_ERR; }
    if !mvfs_range_ok(name_ptr, name_len) { return MVFS_ERR; }
    if !mvfs_range_ok(buf_ptr, buf_max) { return MVFS_ERR; }

    let name = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_max as usize) };

    match crate::fs::mvfs::read(name, buf) {
        Ok(n) => n as u64,
        Err(_) => MVFS_ERR,
    }
}

pub fn syscall_mvfs_delete(name_ptr: u64, name_len: u64) -> u64 {
    if name_len == 0 || name_len > crate::fs::mvfs::MVFS_MAX_NAME as u64 { return MVFS_ERR; }
    if !mvfs_range_ok(name_ptr, name_len) { return MVFS_ERR; }

    let name = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, name_len as usize) };
    if crate::fs::mvfs::delete(name) { 0 } else { MVFS_ERR }
}

pub fn syscall_mvfs_list(prefix_ptr: u64, prefix_len: u64, buf_ptr: u64, buf_max: u64) -> u64 {
    if buf_max == 0 || buf_max > 4096 { return MVFS_ERR; }
    if !mvfs_range_ok(buf_ptr, buf_max) { return MVFS_ERR; }
    if prefix_len > crate::fs::mvfs::MVFS_MAX_NAME as u64 { return MVFS_ERR; }
    // Empty prefix (prefix_len == 0) is legitimate — means "list all".
    // Only require a valid pointer if we're actually going to read.
    if prefix_len > 0 && !mvfs_range_ok(prefix_ptr, prefix_len) { return MVFS_ERR; }

    let prefix: &[u8] = if prefix_len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(prefix_ptr as *const u8, prefix_len as usize) }
    };
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, buf_max as usize) };
    crate::fs::mvfs::list(prefix, buf) as u64
}
