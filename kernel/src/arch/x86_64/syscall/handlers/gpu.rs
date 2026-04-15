//! GPU syscalls: framebuffer flush + display info / mapping.

pub fn syscall_gpu_flush(x: u64, y: u64, w: u64, h: u64) -> u64 {
    crate::drivers::virtio_gpu::flush_rect(x as u32, y as u32, w as u32, h as u32);
    0
}

pub fn syscall_gpu_info(virt_addr: u64) -> u64 {
    use crate::drivers::virtio_gpu;

    if !virtio_gpu::GPU_ACTIVE.load(core::sync::atomic::Ordering::Relaxed) {
        return u64::MAX;
    }

    let (width, height) = match virtio_gpu::display_size() {
        Some(wh) => wh,
        None => return u64::MAX,
    };

    let pages = match virtio_gpu::framebuffer_pages() {
        Some(p) => p,
        None => return u64::MAX,
    };

    let task_id = crate::task::task::get_current_task();
    if let Some(task_arc) = crate::task::task::get_task(task_id) {
        let pml4_phys = task_arc.lock().page_table_phys;
        let flags = x86_64::structures::paging::PageTableFlags::PRESENT
            | x86_64::structures::paging::PageTableFlags::WRITABLE
            | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE
            | x86_64::structures::paging::PageTableFlags::NO_EXECUTE
            | x86_64::structures::paging::PageTableFlags::WRITE_THROUGH;

        for (i, &phys_page) in pages.iter().enumerate() {
            let virt = virt_addr as usize + i * 4096;
            let _ = crate::memory::paging::map_page_in_table(
                pml4_phys, virt, phys_page, flags
            );
        }
    }

    ((width as u64) << 32) | (height as u64)
}
