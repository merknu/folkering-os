//! DMA / IOMMU / WASM network bridge syscalls (Phase 10/11).
//!
//! - DMA buffer allocation + IOMMU status query
//! - WASM net driver bridge: register, submit_rx, poll_tx, dma_rx, metrics
//! - DMA sync read/write (HHDM-based, with explicit cache flush for WHPX)

pub fn syscall_dma_alloc(size: u64, vaddr: u64) -> u64 {
    let num_pages = ((size as usize) + 4095) / 4096;
    if num_pages == 0 || num_pages > 256 {
        return u64::MAX;
    }
    if vaddr < 0x200000 || vaddr >= 0xFFFF_8000_0000_0000 {
        return u64::MAX;
    }

    let phys_addr = match crate::memory::physical::alloc_contiguous(num_pages) {
        Some(addr) => addr,
        None => {
            crate::serial_strln!("[DMA] Failed to allocate contiguous memory");
            return u64::MAX;
        }
    };

    use crate::memory::paging;
    use crate::task::task::{get_current_task, get_task};
    use x86_64::structures::paging::PageTableFlags as Ptf;
    let task_id = get_current_task();
    let pml4_phys = match get_task(task_id) {
        Some(task) => task.lock().page_table_phys,
        None => return u64::MAX,
    };

    let ptf = Ptf::PRESENT | Ptf::WRITABLE | Ptf::USER_ACCESSIBLE | Ptf::NO_EXECUTE
        | Ptf::WRITE_THROUGH | Ptf::NO_CACHE;

    for i in 0..num_pages {
        let virt = vaddr as usize + i * 4096;
        let phys = phys_addr + i * 4096;
        if paging::map_page_in_table(pml4_phys, virt, phys, ptf).is_err() {
            crate::serial_strln!("[DMA] Page mapping failed");
            return u64::MAX;
        }
    }

    let iommu = crate::arch::x86_64::acpi::iommu_available();

    crate::serial_str!("[DMA] Allocated ");
    crate::drivers::serial::write_dec(num_pages as u32);
    crate::serial_str!(" pages at phys=");
    crate::drivers::serial::write_hex(phys_addr as u64);
    crate::serial_str!(" vaddr=");
    crate::drivers::serial::write_hex(vaddr);
    if iommu {
        crate::serial_str!(" (IOMMU available)");
    }
    crate::drivers::serial::write_newline();

    phys_addr as u64
}

pub fn syscall_iommu_status() -> u64 {
    let available = crate::arch::x86_64::acpi::iommu_available();
    let base = crate::arch::x86_64::acpi::iommu_base();
    if available {
        (base & 0xFFFFFFFF_00000000) | 1
    } else {
        0
    }
}

pub fn syscall_net_register(mac_lo: u64, mac_hi: u64) -> u64 {
    let mac = [
        (mac_lo & 0xFF) as u8,
        ((mac_lo >> 8) & 0xFF) as u8,
        ((mac_lo >> 16) & 0xFF) as u8,
        ((mac_lo >> 24) & 0xFF) as u8,
        (mac_hi & 0xFF) as u8,
        ((mac_hi >> 8) & 0xFF) as u8,
    ];
    crate::net::init_wasm_net(mac);
    0
}

pub fn syscall_net_submit_rx(vaddr: u64, length: u64) -> u64 {
    let len = length as usize;
    if len == 0 || len > 1514 || vaddr < 0x200000 {
        return u64::MAX;
    }
    let data = unsafe {
        core::slice::from_raw_parts(vaddr as *const u8, len)
    };
    if crate::net::wasm_net_submit_rx(data) {
        0
    } else {
        u64::MAX
    }
}

pub fn syscall_net_poll_tx(vaddr: u64, max_len: u64) -> u64 {
    let max = max_len as usize;
    if max == 0 || max > 2048 || vaddr < 0x200000 {
        return u64::MAX;
    }
    let buf = unsafe {
        core::slice::from_raw_parts_mut(vaddr as *mut u8, max)
    };
    match crate::net::wasm_net_poll_tx(buf) {
        Some(len) => {
            static TX_POP_LOG: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
            let c = TX_POP_LOG.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if c < 5 {
                crate::serial_str!("[NET-POP] ");
                crate::drivers::serial::write_dec(len as u32);
                crate::serial_strln!("B popped from TX ring");
            }
            len as u64
        }
        None => 0,
    }
}

pub fn syscall_dma_sync_read(phys_addr: u64, dest_and_len: u64) -> u64 {
    if phys_addr == 0 { return u64::MAX; }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let src_virt = hhdm + phys_addr as usize;

    let len = ((dest_and_len >> 32) & 0xFFFF) as usize;

    if len == 0 {
        // Mode 2: read u64 directly — flush cache line first to see DMA writes
        unsafe {
            core::arch::asm!("clflush [{}]", in(reg) src_virt, options(nostack));
            core::arch::asm!("mfence", options(nostack));
        }
        let val = unsafe { core::ptr::read_volatile(src_virt as *const u64) };
        return val;
    }

    // Mode 1: bulk copy
    let dest_vaddr = (dest_and_len & 0xFFFFFFFF) as usize;
    if len > 4096 || dest_vaddr < 0x200000 {
        return u64::MAX;
    }

    let src = src_virt as *const u8;
    let dst = dest_vaddr as *mut u8;
    unsafe {
        let mut addr = src_virt;
        while addr < src_virt + len {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));

        for i in 0..len {
            let byte = core::ptr::read_volatile(src.add(i));
            core::ptr::write_volatile(dst.add(i), byte);
        }
    }

    len as u64
}

pub fn syscall_net_dma_rx(ring_and_idx: u64, buf_and_size: u64) -> u64 {
    let ring_phys = ring_and_idx & 0x0000_FFFF_FFFF_FFFF;
    let desc_idx = ((ring_and_idx >> 48) & 0xFFFF) as usize;
    let buf_phys = buf_and_size & 0x0000_FFFF_FFFF_FFFF;
    let buf_size = ((buf_and_size >> 48) & 0xFFFF) as usize;

    if ring_phys == 0 || buf_phys == 0 || buf_size == 0 || desc_idx > 7 {
        return u64::MAX;
    }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);

    let desc_phys = ring_phys + (desc_idx as u64 * 16);
    let desc_virt = hhdm + desc_phys as usize;

    unsafe {
        core::arch::asm!("clflush [{}]", in(reg) desc_virt, options(nostack));
        core::arch::asm!("mfence", options(nostack));
    }

    let len_status = unsafe { core::ptr::read_volatile((desc_virt + 8) as *const u64) };
    let pkt_len = (len_status & 0xFFFF) as usize;

    if pkt_len == 0 || pkt_len > 2048 {
        return 0;
    }

    let pkt_phys = buf_phys + (desc_idx as u64 * buf_size as u64);
    let pkt_virt = hhdm + pkt_phys as usize;

    unsafe {
        let mut addr = pkt_virt;
        let end = pkt_virt + pkt_len;
        while addr < end {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));
    }

    let mut pkt_buf = [0u8; 2048];
    unsafe {
        let src = pkt_virt as *const u8;
        for i in 0..pkt_len {
            pkt_buf[i] = core::ptr::read_volatile(src.add(i));
        }
    }

    if crate::net::wasm_net_submit_rx(&pkt_buf[..pkt_len]) {
        pkt_len as u64
    } else {
        0
    }
}

pub fn syscall_dma_sync_write(phys_addr: u64, src_and_len: u64) -> u64 {
    let src_vaddr = (src_and_len & 0xFFFFFFFF) as usize;
    let len = ((src_and_len >> 32) & 0xFFFF) as usize;

    if len == 0 || len > 4096 || phys_addr == 0 || src_vaddr < 0x200000 {
        return u64::MAX;
    }

    let hhdm = crate::HHDM_OFFSET.load(core::sync::atomic::Ordering::Relaxed);
    let dst_virt = hhdm + phys_addr as usize;
    let src = src_vaddr as *const u8;
    let dst = dst_virt as *mut u8;

    unsafe {
        for i in 0..len {
            let byte = core::ptr::read_volatile(src.add(i));
            core::ptr::write_volatile(dst.add(i), byte);
        }
        let mut addr = dst_virt;
        while addr < dst_virt + len {
            core::arch::asm!("clflush [{}]", in(reg) addr, options(nostack));
            addr += 64;
        }
        core::arch::asm!("mfence", options(nostack));
    }

    len as u64
}

pub fn syscall_net_metrics(metric_id: u64, _reserved: u64) -> u64 {
    match metric_id {
        0 => {
            // Network: has_ip(1) | ip_a(8) | ip_b(8) | ip_c(8) | ip_d(8)
            let has_ip = if crate::net::has_ip() { 1u64 } else { 0u64 };
            let guard = crate::net::NET_STATE.lock();
            if let Some(ref state) = *guard {
                let addrs = state.iface.ip_addrs();
                if let Some(cidr) = addrs.first() {
                    if let smoltcp::wire::IpAddress::Ipv4(v4) = cidr.address() {
                        let o = v4.octets();
                        drop(guard);
                        return has_ip
                            | ((o[0] as u64) << 8)
                            | ((o[1] as u64) << 16)
                            | ((o[2] as u64) << 24)
                            | ((o[3] as u64) << 32);
                    }
                }
            }
            drop(guard);
            has_ip
        }
        1 => {
            // Firewall: allows(32) | drops(32)
            let allows = crate::net::firewall::ALLOWS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            let drops = crate::net::firewall::DROPS.load(core::sync::atomic::Ordering::Relaxed) as u64;
            allows | (drops << 32)
        }
        2 => crate::timer::uptime_ms(),
        3 => crate::net::firewall::SUSPICIOUS.count.load(core::sync::atomic::Ordering::Relaxed) as u64,
        4 => {
            // Anomaly detection stats: blocked_ips(16) | total_syn_attempts(16)
            let (blocked, attempts) = crate::net::firewall::anomaly_stats();
            (blocked as u64) | ((attempts as u64) << 16)
        }
        _ => u64::MAX,
    }
}
