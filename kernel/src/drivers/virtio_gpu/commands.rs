//! VirtIO-GPU command structures and submission helpers.
//!
//! Defines the wire format used by the device control queue, plus
//! `submit_and_wait` (synchronous, init only) and `recycle_used`.

use crate::drivers::virtio::{Virtqueue, VRING_DESC_F_NEXT, VRING_DESC_F_WRITE};
use super::GpuState;

// ── VirtIO GPU Command Types ───────────────────────────────────────────
// Control queue commands (sent on queue 0):

pub(super) const VIRTIO_GPU_CMD_GET_DISPLAY_INFO: u32 = 0x0100;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
pub(super) const VIRTIO_GPU_CMD_SET_SCANOUT: u32 = 0x0103;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0104;
pub(super) const VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
pub(super) const VIRTIO_GPU_CMD_RESOURCE_FLUSH: u32 = 0x0106;
// RESOURCE_BLOB family — only valid if VIRTIO_GPU_F_RESOURCE_BLOB negotiated.
// `resources::create_framebuffer_blob` is gated on `state.has_resource_blob`;
// the legacy CREATE_2D path is the fallback.
pub(super) const VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB: u32 = 0x010C;
pub(super) const VIRTIO_GPU_CMD_SET_SCANOUT_BLOB: u32 = 0x010D;

// Cursor queue commands (sent on queue 1, dedicated so cursor stays
// responsive when controlq is backed up with heavy renders):
pub(super) const VIRTIO_GPU_CMD_UPDATE_CURSOR: u32 = 0x0300;
pub(super) const VIRTIO_GPU_CMD_MOVE_CURSOR: u32 = 0x0301;

pub(super) const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;

// ── RESOURCE_BLOB constants ────────────────────────────────────────────
// Mirrors `virtio_gpu.h` from the spec. We use BLOB_MEM_GUEST for the
// scanout framebuffer (host reads guest pages directly). HOST3D and
// USE_MAPPABLE are reserved for future virgl/3D paths and stay defined
// for callers, even though we don't take them today.
pub(super) const VIRTIO_GPU_BLOB_MEM_GUEST: u32 = 0x0001;
#[allow(dead_code)]
pub(super) const VIRTIO_GPU_BLOB_MEM_HOST3D: u32 = 0x0002;
#[allow(dead_code)]
pub(super) const VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;

// ── VirtIO GPU Pixel Formats ───────────────────────────────────────────

pub(super) const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2; // BGRX 32-bit

// ── Wire Structures ────────────────────────────────────────────────────

#[repr(C)]
pub(super) struct GpuCtrlHdr {
    pub(super) cmd_type: u32,
    pub(super) flags: u32,
    pub(super) fence_id: u64,
    pub(super) ctx_id: u32,
    pub(super) _padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct GpuRect {
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[repr(C)]
pub(super) struct GpuGetDisplayInfo {
    pub(super) hdr: GpuCtrlHdr,
}

#[repr(C)]
pub(super) struct GpuRespDisplayInfo {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) pmodes: [GpuDisplayOne; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct GpuDisplayOne {
    pub(super) r: GpuRect,
    pub(super) enabled: u32,
    pub(super) flags: u32,
}

#[repr(C)]
pub(super) struct GpuResourceCreate2D {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) resource_id: u32,
    pub(super) format: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[repr(C)]
pub(super) struct GpuResourceAttachBacking {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) resource_id: u32,
    pub(super) nr_entries: u32,
    // Followed by nr_entries × GpuMemEntry
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct GpuMemEntry {
    pub(super) addr: u64,
    pub(super) length: u32,
    pub(super) padding: u32,
}

#[repr(C)]
pub(super) struct GpuSetScanout {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) r: GpuRect,
    pub(super) scanout_id: u32,
    pub(super) resource_id: u32,
}

#[repr(C)]
pub(super) struct GpuTransferToHost2D {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) r: GpuRect,
    pub(super) offset: u64,
    pub(super) resource_id: u32,
    pub(super) padding: u32,
}

#[repr(C)]
pub(super) struct GpuResourceFlush {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) r: GpuRect,
    pub(super) resource_id: u32,
    pub(super) padding: u32,
}

#[repr(C)]
pub(super) struct GpuRespHdr {
    pub(super) cmd_type: u32,
    pub(super) flags: u32,
    pub(super) fence_id: u64,
    pub(super) ctx_id: u32,
    pub(super) _padding: u32,
}

// ── Cursor queue wire format ───────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct GpuCursorPos {
    pub(super) scanout_id: u32,
    pub(super) x: u32,
    pub(super) y: u32,
    pub(super) padding: u32,
}

/// Used for both UPDATE_CURSOR (sets sprite + position) and MOVE_CURSOR
/// (position only — `resource_id`, `hot_x`, `hot_y` are ignored). The shared
/// struct is what the spec defines and matches what QEMU expects on cursorq.
#[repr(C)]
pub(super) struct GpuUpdateCursor {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) pos: GpuCursorPos,
    pub(super) resource_id: u32,
    pub(super) hot_x: u32,
    pub(super) hot_y: u32,
    pub(super) padding: u32,
}

// ── RESOURCE_BLOB wire format ──────────────────────────────────────────

#[repr(C)]
pub(super) struct GpuResourceCreateBlob {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) resource_id: u32,
    pub(super) blob_mem: u32,
    pub(super) blob_flags: u32,
    pub(super) nr_entries: u32,
    pub(super) blob_id: u64,
    pub(super) size: u64,
    // Followed by nr_entries × GpuMemEntry (same layout as ATTACH_BACKING).
}

#[repr(C)]
pub(super) struct GpuSetScanoutBlob {
    pub(super) hdr: GpuCtrlHdr,
    pub(super) r: GpuRect,
    pub(super) scanout_id: u32,
    pub(super) resource_id: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) format: u32,
    pub(super) padding: u32,
    pub(super) strides: [u32; 4],
    pub(super) offsets: [u32; 4],
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Build a zeroed `GpuCtrlHdr` with the given command type.
pub(super) fn make_hdr(cmd_type: u32) -> GpuCtrlHdr {
    GpuCtrlHdr {
        cmd_type,
        flags: 0,
        fence_id: 0,
        ctx_id: 0,
        _padding: 0,
    }
}

/// Submit a command synchronously (for init only). Blocks until completion.
pub(super) fn submit_and_wait(
    state: &mut GpuState,
    cmd_phys: usize,
    cmd_size: usize,
    resp_phys: usize,
    resp_size: usize,
) -> Result<(), &'static str> {
    let q = &mut state.controlq;
    recycle_used(q);

    let d0 = q.alloc_desc().ok_or("no descriptors")?;
    let d1 = q.alloc_desc().ok_or("no descriptors")?;

    q.set_desc(d0, cmd_phys as u64, cmd_size as u32, VRING_DESC_F_NEXT, d1);
    q.set_desc(d1, resp_phys as u64, resp_size as u32, VRING_DESC_F_WRITE, 0);

    crate::serial_str!("[VIRTIO_GPU] submit: d0=");
    crate::drivers::serial::write_dec(d0 as u32);
    crate::serial_str!(" d1=");
    crate::drivers::serial::write_dec(d1 as u32);
    crate::serial_str!(" cmd_phys=");
    crate::drivers::serial::write_hex(cmd_phys as u64);
    crate::serial_str!(" cmd_size=");
    crate::drivers::serial::write_dec(cmd_size as u32);
    crate::serial_str!(" resp_phys=");
    crate::drivers::serial::write_hex(resp_phys as u64);
    crate::drivers::serial::write_newline();

    crate::serial_str!("[VIRTIO_GPU] desc_phys=");
    crate::drivers::serial::write_hex(q.desc_phys());
    crate::serial_str!(" avail_phys=");
    crate::drivers::serial::write_hex(q.avail_phys());
    crate::serial_str!(" used_phys=");
    crate::drivers::serial::write_hex(q.used_phys());
    crate::drivers::serial::write_newline();

    q.submit(d0);

    crate::serial_str!("[VIRTIO_GPU] Notifying queue 0...\n");
    state.transport.notify_queue(0);

    crate::serial_str!("[VIRTIO_GPU] avail_idx=");
    crate::drivers::serial::write_dec(q.next_avail as u32);
    crate::serial_str!(" last_used=");
    crate::drivers::serial::write_dec(q.last_used_idx as u32);
    crate::serial_str!(" dev_used=");
    crate::drivers::serial::write_dec(q.used_idx() as u32);
    crate::serial_str!(" q_size=");
    crate::drivers::serial::write_dec(q.queue_size as u32);
    crate::drivers::serial::write_newline();
    crate::serial_str!("[VIRTIO_GPU] Waiting for used ring (polling)...\n");

    // Wait for completion (init only — blocking OK)
    for i in 0..100_000_000u64 {
        if q.has_used() {
            crate::serial_str!("[VIRTIO_GPU] Got used at iteration ");
            crate::drivers::serial::write_dec(i as u32);
            crate::drivers::serial::write_newline();
            recycle_used(q);
            return Ok(());
        }
        if i % 10_000_000 == 0 && i > 0 {
            crate::serial_str!("[VIRTIO_GPU] Still waiting... ");
            crate::drivers::serial::write_dec((i / 1_000_000) as u32);
            crate::serial_str!("M iters\n");
        }
        core::hint::spin_loop();
    }

    crate::serial_str!("[VIRTIO_GPU] TIMEOUT after 100M iterations\n");
    Err("GPU command timeout")
}

/// Recycle all used descriptors back to free pool.
pub(super) fn recycle_used(q: &mut Virtqueue) {
    while q.has_used() {
        if let Some((head_idx, _len)) = q.pop_used() {
            // Free the descriptor chain
            let mut idx = head_idx;
            loop {
                let next = q.desc_next(idx);
                let has_next = q.desc_has_next(idx);
                q.free_desc(idx);
                if !has_next { break; }
                idx = next;
            }
        }
    }
}
