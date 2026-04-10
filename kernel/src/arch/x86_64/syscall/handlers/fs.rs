//! Filesystem syscalls: ramdisk read_dir/read_file + raw block I/O.

// ── Block Device ───────────────────────────────────────────────────────

pub fn syscall_block_read(sector: u64, buf_ptr: u64, count: u64) -> u64 {
    use crate::drivers::virtio_blk;

    if !virtio_blk::is_initialized() {
        return u64::MAX;
    }

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX;
    }

    if buf_ptr >= 0x0000_8000_0000_0000 {
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

    if buf_ptr == 0 || count == 0 || count > 128 {
        return u64::MAX;
    }

    let buf_len = (count as usize) * virtio_blk::SECTOR_SIZE;

    if buf_ptr >= 0x0000_8000_0000_0000 {
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

    if buf_ptr == 0 || buf_size == 0 {
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
    if name_ptr == 0 || buf_ptr == 0 || buf_size == 0 {
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
