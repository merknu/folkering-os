//! Memory syscalls: shared memory (Phase 6), anonymous mmap (Phase 9),
//! and physical memory mapping (Phase 6.2).

/// Map physical memory flags
pub mod map_flags {
    /// Allow reading from mapped memory
    pub const MAP_READ: u64 = 0x01;
    /// Allow writing to mapped memory
    pub const MAP_WRITE: u64 = 0x02;
    /// Allow executing from mapped memory (usually not used for MMIO)
    pub const MAP_EXEC: u64 = 0x04;
    /// Use Write-Combining caching (PAT index 4) - for framebuffer
    pub const MAP_CACHE_WC: u64 = 0x10;
    /// Use Uncached mode - for MMIO devices
    pub const MAP_CACHE_UC: u64 = 0x20;
}

// ── Shared Memory ──────────────────────────────────────────────────────

pub fn syscall_shmem_create(size: u64) -> u64 {
    use crate::ipc::shared_memory::{shmem_create, ShmemPerms};

    if size == 0 || size > 1024 * 1024 * 1024 {
        return u64::MAX;
    }

    match shmem_create(size as usize, ShmemPerms::ReadWrite) {
        Ok(shmem_id) => shmem_id.get() as u64,
        Err(_) => u64::MAX,
    }
}

pub fn syscall_shmem_map(shmem_id: u64, virt_addr: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_map;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    if virt_addr == 0 {
        return u64::MAX;
    }

    match shmem_map(id, virt_addr as usize) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

pub fn syscall_shmem_grant(shmem_id: u64, target_task: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_grant;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    if target_task == 0 || target_task > u32::MAX as u64 {
        return u64::MAX;
    }

    match shmem_grant(id, target_task as u32) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

pub fn syscall_shmem_unmap(shmem_id: u64, virt_addr: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_unmap;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    if virt_addr == 0 {
        return u64::MAX;
    }

    match shmem_unmap(id, virt_addr as usize) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

pub fn syscall_shmem_destroy(shmem_id: u64) -> u64 {
    use crate::ipc::shared_memory::shmem_destroy;
    use core::num::NonZeroU32;

    let id = match NonZeroU32::new(shmem_id as u32) {
        Some(id) => id,
        None => return u64::MAX,
    };

    match shmem_destroy(id) {
        Ok(()) => 0,
        Err(_) => u64::MAX,
    }
}

// ── Anonymous Memory Mapping ───────────────────────────────────────────

pub fn syscall_mmap(hint_addr: u64, size: u64, flags: u64) -> u64 {
    use crate::memory::physical::alloc_page;
    use crate::memory::paging::map_page_in_table;
    use x86_64::structures::paging::PageTableFlags;

    const PAGE_SIZE: u64 = 4096;
    const MAX_MMAP_SIZE: u64 = 16 * 1024 * 1024;
    const MMAP_BASE: u64 = 0x4000_0000;

    if size == 0 || size > MAX_MMAP_SIZE {
        return u64::MAX;
    }

    let num_pages = ((size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return u64::MAX;
    }

    let virt_base = if hint_addr != 0 {
        if hint_addr % PAGE_SIZE != 0 || hint_addr < MMAP_BASE {
            return u64::MAX;
        }
        hint_addr
    } else {
        use core::sync::atomic::{AtomicU64, Ordering};
        static NEXT_MMAP_ADDR: AtomicU64 = AtomicU64::new(MMAP_BASE);
        let addr = NEXT_MMAP_ADDR.fetch_add(num_pages as u64 * PAGE_SIZE, Ordering::Relaxed);
        if addr + (num_pages as u64 * PAGE_SIZE) > 0x7FFF_0000_0000 {
            return u64::MAX;
        }
        addr
    };

    let mut pt_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if flags & 0x2 != 0 {
        pt_flags |= PageTableFlags::WRITABLE;
    }
    if flags & 0x4 == 0 {
        pt_flags |= PageTableFlags::NO_EXECUTE;
    }

    for i in 0..num_pages {
        let phys = match alloc_page() {
            Some(p) => p,
            None => return u64::MAX,
        };

        let virt = virt_base + (i as u64 * PAGE_SIZE);
        if map_page_in_table(task_pml4, virt as usize, phys, pt_flags).is_err() {
            return u64::MAX;
        }

        let hhdm_ptr = crate::phys_to_virt(phys) as *mut u8;
        unsafe {
            core::ptr::write_bytes(hhdm_ptr, 0, PAGE_SIZE as usize);
        }
    }

    virt_base
}

pub fn syscall_munmap(virt_addr: u64, size: u64) -> u64 {
    use crate::memory::paging::unmap_page_in_table;
    use crate::memory::physical::free_pages;

    const PAGE_SIZE: u64 = 4096;
    const MMAP_BASE: u64 = 0x4000_0000;

    if size == 0 || virt_addr % PAGE_SIZE != 0 || virt_addr < MMAP_BASE {
        return u64::MAX;
    }

    let num_pages = ((size + PAGE_SIZE - 1) / PAGE_SIZE) as usize;

    let task_pml4 = crate::task::task::current_task().lock().page_table_phys;
    if task_pml4 == 0 {
        return u64::MAX;
    }

    let mut freed = 0usize;
    for i in 0..num_pages {
        let virt = virt_addr + (i as u64 * PAGE_SIZE);
        if let Ok(phys_addr) = unmap_page_in_table(task_pml4, virt as usize) {
            free_pages(phys_addr, 0);
            freed += 1;
        }
    }

    if freed > 0 {
        crate::serial_println!("[MUNMAP] Freed {} pages at {:#x}", freed, virt_addr);
    }

    0
}

// ── Physical Memory Mapping (Phase 6.2) ────────────────────────────────

pub fn syscall_map_physical(phys_addr: u64, virt_addr: u64, size: u64, flags: u64, _reserved: u64) -> u64 {
    use crate::capability;
    use crate::memory::paging;
    use crate::task::task::{get_current_task, get_task};
    use x86_64::structures::paging::PageTableFlags as PTF;

    let task_id = get_current_task();

    if phys_addr & 0xFFF != 0 || virt_addr & 0xFFF != 0 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Address not page-aligned");
        return u64::MAX;
    }

    if virt_addr >= 0x8000_0000_0000_0000 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Virtual address in kernel space");
        return u64::MAX;
    }

    if size == 0 || size > 256 * 1024 * 1024 {
        crate::serial_println!("[MAP_PHYSICAL] Error: Invalid size");
        return u64::MAX;
    }

    // PCI MMIO BARs are typically above 0xF0000000 (MMIO hole)
    let is_pci_mmio = phys_addr >= 0xF000_0000 && size <= 1024 * 1024;
    if !is_pci_mmio && !capability::has_framebuffer_access(task_id, phys_addr, size) {
        crate::serial_str!("[MAP_PHYSICAL] Error: No capability for task ");
        crate::drivers::serial::write_dec(task_id);
        crate::serial_str!(" phys=");
        crate::drivers::serial::write_hex(phys_addr);
        crate::drivers::serial::write_newline();
        return u64::MAX;
    }

    let pml4_phys = match get_task(task_id) {
        Some(task) => task.lock().page_table_phys,
        None => {
            crate::serial_println!("[MAP_PHYSICAL] Error: Task not found");
            return u64::MAX;
        }
    };

    let mut ptf = PTF::PRESENT.union(PTF::USER_ACCESSIBLE).union(PTF::NO_EXECUTE);

    if flags & map_flags::MAP_WRITE != 0 {
        ptf = ptf.union(PTF::WRITABLE);
    }

    if flags & map_flags::MAP_CACHE_WC != 0 {
        ptf = ptf.union(PTF::NO_CACHE);
        crate::serial_println!("[MAP_PHYSICAL] Note: WC requested but using UC (PAT not supported by crate)");
    } else if flags & map_flags::MAP_CACHE_UC != 0 {
        ptf = ptf.union(PTF::NO_CACHE).union(PTF::WRITE_THROUGH);
    }

    let num_pages = ((size + 0xFFF) / 0x1000) as usize;

    crate::serial_println!("[MAP_PHYSICAL] Mapping {} pages from phys {:#x} to virt {:#x}",
                          num_pages, phys_addr, virt_addr);

    for i in 0..num_pages {
        let phys = phys_addr as usize + i * 0x1000;
        let virt = virt_addr as usize + i * 0x1000;

        if paging::map_page_in_table(pml4_phys, virt, phys, ptf).is_err() {
            crate::serial_println!("[MAP_PHYSICAL] Error: Failed to map page at {:#x}", virt);
            return u64::MAX;
        }
    }

    crate::serial_println!("[MAP_PHYSICAL] Successfully mapped {} pages", num_pages);
    0
}
