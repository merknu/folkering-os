//! Hot-path flush functions for VirtIO-GPU.
//!
//! - `flush_rect` — single rect, async (TRANSFER + FLUSH, no wait)
//! - `flush_rects_batched` — up to 4 rects, ONE doorbell
//! - `flush_and_vsync` — like flush_rect, then HLT until fence completes

use crate::drivers::virtio::{VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};
use crate::memory::physical;

use super::commands::*;
use super::io::VIRTIO_GPU_FLAG_FENCE;
use super::{
    GPU_STATE, FENCE_COUNTER, FENCE_COMPLETE, FLUSH_CMD_PAGE,
};

/// Flush a dirty rectangle to the display (hot path).
/// Non-blocking: submits TRANSFER_TO_HOST_2D + RESOURCE_FLUSH, returns immediately.
/// Called from SYS_GPU_FLUSH syscall.
pub fn flush_rect(x: u32, y: u32, w: u32, h: u32) {
    let submit_tsc = crate::drivers::iqe::rdtsc();

    // Debug: count flushes (first 3 only)
    static FLUSH_N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let fc = FLUSH_N.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if fc < 3 {
        crate::drivers::serial::com3_write(b"IQE,FLUSH,1\n");
    }

    let mut guard = GPU_STATE.lock();
    let state = match guard.as_mut() {
        Some(s) if s.active => s,
        _ => return,
    };

    // Recycle used descriptors before submitting new ones
    recycle_used(&mut state.controlq);

    // Reuse static command page (allocate once, reuse forever)
    let cmd_phys = {
        let mut page_guard = FLUSH_CMD_PAGE.lock();
        if page_guard.is_none() {
            *page_guard = physical::alloc_page();
        }
        match *page_guard {
            Some(p) => p,
            None => return,
        }
    };
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    // Layout: [TransferToHost2D @ 0] [ResourceFlush @ 64] [RespHdr @ 128] [RespHdr @ 160]
    unsafe {
        // TRANSFER_TO_HOST_2D
        let transfer = cmd_virt as *mut GpuTransferToHost2D;
        (*transfer).hdr = make_hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
        (*transfer).r = GpuRect { x, y, width: w, height: h };
        (*transfer).offset = 0;
        (*transfer).resource_id = 1;
        (*transfer).padding = 0;

        // RESOURCE_FLUSH with VSync fence
        let fence_id = FENCE_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let flush = cmd_virt.add(64) as *mut GpuResourceFlush;
        (*flush).hdr = GpuCtrlHdr {
            cmd_type: VIRTIO_GPU_CMD_RESOURCE_FLUSH,
            flags: VIRTIO_GPU_FLAG_FENCE,
            fence_id,
            ctx_id: 0,
            _padding: 0,
        };
        (*flush).r = GpuRect { x, y, width: w, height: h };
        (*flush).resource_id = 1;
        (*flush).padding = 0;

        // Response buffers (zeroed)
        core::ptr::write_bytes(cmd_virt.add(128), 0, 64);
    }

    // Submit: 4 chained descriptors (transfer cmd, resp1, flush cmd, resp2)
    let q = &mut state.controlq;
    if let Some(d0) = q.alloc_desc() {
        if let Some(d1) = q.alloc_desc() {
            if let Some(d2) = q.alloc_desc() {
                if let Some(d3) = q.alloc_desc() {
                    // Transfer command
                    q.set_desc(d0, cmd_phys as u64,
                        core::mem::size_of::<GpuTransferToHost2D>() as u32,
                        VRING_DESC_F_NEXT, d1);
                    // Transfer response
                    q.set_desc(d1, (cmd_phys + 128) as u64,
                        core::mem::size_of::<GpuRespHdr>() as u32,
                        VRING_DESC_F_WRITE, 0);
                    // Flush command
                    q.set_desc(d2, (cmd_phys + 64) as u64,
                        core::mem::size_of::<GpuResourceFlush>() as u32,
                        VRING_DESC_F_NEXT, d3);
                    // Flush response
                    q.set_desc(d3, (cmd_phys + 160) as u64,
                        core::mem::size_of::<GpuRespHdr>() as u32,
                        VRING_DESC_F_WRITE, 0);

                    // Submit both as separate available ring entries
                    q.submit(d0);
                    q.submit(d2);

                    // Ring doorbell (async — don't wait)
                    state.transport.notify_queue(0);
                } else { q.free_desc(d2); q.free_desc(d1); q.free_desc(d0); }
            } else { q.free_desc(d1); q.free_desc(d0); }
        } else { q.free_desc(d0); }
    }

    // IQE: always record flush submit (even if descriptors exhausted)
    crate::drivers::iqe::record(
        crate::drivers::iqe::IqeEventType::GpuFlushSubmit,
        submit_tsc,
        0,
    );
}

/// Batched flush: transfer + flush multiple rects with ONE doorbell (ONE VM-exit).
/// Each rect gets its own TRANSFER_TO_HOST_2D, all share one RESOURCE_FLUSH.
/// Max 4 rects per batch (limited by command page layout).
pub fn flush_rects_batched(rects: &[(u32, u32, u32, u32)]) {
    if rects.is_empty() { return; }
    let submit_tsc = crate::drivers::iqe::rdtsc();

    let mut guard = GPU_STATE.lock();
    let state = match guard.as_mut() {
        Some(s) if s.active => s,
        _ => return,
    };
    recycle_used(&mut state.controlq);

    let cmd_phys = {
        let mut pg = FLUSH_CMD_PAGE.lock();
        if pg.is_none() { *pg = physical::alloc_page(); }
        match *pg { Some(p) => p, None => return }
    };
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd = (hhdm + cmd_phys) as *mut u8;

    let n = rects.len().min(4); // max 4 rects per batch

    // Compute union bounding box for the final RESOURCE_FLUSH
    let mut ux = rects[0].0;
    let mut uy = rects[0].1;
    let mut ur = ux + rects[0].2;
    let mut ub = uy + rects[0].3;
    for r in &rects[1..n] {
        ux = ux.min(r.0);
        uy = uy.min(r.1);
        ur = ur.max(r.0 + r.2);
        ub = ub.max(r.1 + r.3);
    }

    // Layout: for each rect i:
    //   [i*80 + 0..56]  = TRANSFER_TO_HOST_2D
    //   [i*80 + 56..80] = Response (24 bytes)
    // After all rects:
    //   [n*80 + 0..48]  = RESOURCE_FLUSH
    //   [n*80 + 48..72] = Response
    unsafe {
        for i in 0..n {
            let off = i * 80;
            let xfer = cmd.add(off) as *mut GpuTransferToHost2D;
            (*xfer).hdr = make_hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
            (*xfer).r = GpuRect { x: rects[i].0, y: rects[i].1, width: rects[i].2, height: rects[i].3 };
            (*xfer).offset = 0;
            (*xfer).resource_id = 1;
            (*xfer).padding = 0;
            // Zero response
            core::ptr::write_bytes(cmd.add(off + 56), 0, 24);
        }

        let flush_off = n * 80;
        let fence_id = FENCE_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let flush = cmd.add(flush_off) as *mut GpuResourceFlush;
        (*flush).hdr = GpuCtrlHdr {
            cmd_type: VIRTIO_GPU_CMD_RESOURCE_FLUSH,
            flags: VIRTIO_GPU_FLAG_FENCE,
            fence_id,
            ctx_id: 0,
            _padding: 0,
        };
        (*flush).r = GpuRect { x: ux, y: uy, width: ur - ux, height: ub - uy };
        (*flush).resource_id = 1;
        (*flush).padding = 0;
        core::ptr::write_bytes(cmd.add(flush_off + 48), 0, 24);
    }

    // Chain all descriptors: [xfer0→resp0] [xfer1→resp1] ... [flush→resp]
    // Submit each transfer pair separately, then flush pair last.
    // All use ONE doorbell notification at the end.
    let q = &mut state.controlq;
    let mut submitted = false;

    for i in 0..n {
        let off = i * 80;
        if let (Some(d_cmd), Some(d_resp)) = (q.alloc_desc(), q.alloc_desc()) {
            q.set_desc(d_cmd, (cmd_phys + off) as u64, 56, VRING_DESC_F_NEXT, d_resp);
            q.set_desc(d_resp, (cmd_phys + off + 56) as u64, 24, VRING_DESC_F_WRITE, 0);
            q.submit(d_cmd);
            submitted = true;
        }
    }

    // Flush command
    let flush_off = n * 80;
    if let (Some(d_cmd), Some(d_resp)) = (q.alloc_desc(), q.alloc_desc()) {
        q.set_desc(d_cmd, (cmd_phys + flush_off) as u64, 48, VRING_DESC_F_NEXT, d_resp);
        q.set_desc(d_resp, (cmd_phys + flush_off + 48) as u64, 24, VRING_DESC_F_WRITE, 0);
        q.submit(d_cmd);
        submitted = true;
    }

    // ONE doorbell for ALL commands
    if submitted {
        state.transport.notify_queue(0);
    }

    drop(guard);

    crate::drivers::iqe::record(
        crate::drivers::iqe::IqeEventType::GpuFlushSubmit, submit_tsc, 0);
}

/// Flush and wait for VSync (fence completion).
/// Blocks until the GPU has finished presenting the frame.
/// Dramatically reduces CPU usage by sleeping via HLT instead of spinning.
pub fn flush_and_vsync(x: u32, y: u32, w: u32, h: u32) {
    // Get the fence_id that flush_rect will use
    let expected_fence = FENCE_COUNTER.load(core::sync::atomic::Ordering::Relaxed);

    // Submit the flush (which now includes VIRTIO_GPU_FLAG_FENCE)
    flush_rect(x, y, w, h);

    // Wait for GPU to complete the fenced flush
    // Enable interrupts so HLT can be woken
    unsafe { core::arch::asm!("sti"); }

    let mut timeout = 500_000u32; // ~500K iterations max
    while FENCE_COMPLETE.load(core::sync::atomic::Ordering::Acquire) < expected_fence {
        unsafe { core::arch::asm!("hlt"); } // CPU sleeps until interrupt

        // Check ISR status register to detect GPU completion
        let guard = GPU_STATE.lock();
        if let Some(state) = guard.as_ref() {
            if state.active {
                // Read ISR register (clears on read, per VirtIO spec)
                let isr = unsafe {
                    core::ptr::read_volatile(state.transport.isr_base as *const u8)
                };
                if isr & 1 != 0 {
                    // Used buffer notification — fence completed
                    FENCE_COMPLETE.store(expected_fence, core::sync::atomic::Ordering::Release);
                    break;
                }
            }
        }
        drop(guard);

        timeout -= 1;
        if timeout == 0 {
            // Timeout — don't hang forever, just continue
            FENCE_COMPLETE.store(expected_fence, core::sync::atomic::Ordering::Release);
            break;
        }
    }
}
