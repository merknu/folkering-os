//! Framebuffer resource setup: display info, RESOURCE_CREATE_2D,
//! ATTACH_BACKING (scatter-gather), and SET_SCANOUT.

use alloc::vec::Vec;

use crate::memory::physical;
use super::GpuState;
use super::commands::*;

pub(super) fn get_display_info(state: &mut GpuState) -> Result<(u32, u32), &'static str> {
    let cmd_phys = physical::alloc_page().ok_or("alloc failed")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        // Command
        let cmd = cmd_virt as *mut GpuGetDisplayInfo;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_GET_DISPLAY_INFO);

        // Response (after command)
        let resp_offset = core::mem::size_of::<GpuGetDisplayInfo>();
        core::ptr::write_bytes(cmd_virt.add(resp_offset), 0,
            core::mem::size_of::<GpuRespDisplayInfo>());
    }

    // Submit synchronously (init only — blocking is OK)
    submit_and_wait(state, cmd_phys,
        core::mem::size_of::<GpuGetDisplayInfo>(),
        cmd_phys + core::mem::size_of::<GpuGetDisplayInfo>(),
        core::mem::size_of::<GpuRespDisplayInfo>())?;

    // Read response
    let resp = unsafe {
        &*((hhdm + cmd_phys + core::mem::size_of::<GpuGetDisplayInfo>())
            as *const GpuRespDisplayInfo)
    };

    if resp.hdr.cmd_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
        return Err("GET_DISPLAY_INFO failed");
    }

    // Find first enabled display
    for pmode in &resp.pmodes {
        if pmode.enabled != 0 && pmode.r.width > 0 && pmode.r.height > 0 {
            return Ok((pmode.r.width, pmode.r.height));
        }
    }

    // Default fallback
    Ok((1024, 768))
}

pub(super) fn create_framebuffer_resource(state: &mut GpuState) -> Result<(), &'static str> {
    let cmd_phys = physical::alloc_page().ok_or("alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuResourceCreate2D;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
        (*cmd).resource_id = 1;
        (*cmd).format = VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
        (*cmd).width = state.width;
        (*cmd).height = state.height;

        let resp_off = core::mem::size_of::<GpuResourceCreate2D>();
        core::ptr::write_bytes(cmd_virt.add(resp_off), 0, 24);
    }

    submit_and_wait(state, cmd_phys,
        core::mem::size_of::<GpuResourceCreate2D>(),
        cmd_phys + core::mem::size_of::<GpuResourceCreate2D>(), 24)
}

pub(super) fn attach_framebuffer_backing(state: &mut GpuState) -> Result<(), &'static str> {
    let fb_size = (state.width * state.height * 4) as usize;
    let num_pages = (fb_size + 4095) / 4096;

    // Allocate physical pages (scatter-gather — NOT contiguous)
    let mut pages = Vec::with_capacity(num_pages);
    for _ in 0..num_pages {
        let page = physical::alloc_page().ok_or("FB page alloc failed")?;
        // Zero the page
        let hhdm = crate::memory::paging::hhdm_offset();
        unsafe { core::ptr::write_bytes((hhdm + page) as *mut u8, 0, 4096); }
        pages.push(page);
    }

    crate::serial_str!("[VIRTIO_GPU] Allocated ");
    crate::drivers::serial::write_dec(num_pages as u32);
    crate::serial_str!(" pages for ");
    crate::drivers::serial::write_dec((fb_size / 1024) as u32);
    crate::serial_str!("KB framebuffer\n");

    // Build ATTACH_BACKING command with scatter-gather list
    // Need: header (24 bytes) + nr_entries × GpuMemEntry (16 bytes each)
    let entries_size = num_pages * core::mem::size_of::<GpuMemEntry>();

    let cmd_phys = physical::alloc_page().ok_or("cmd alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuResourceAttachBacking;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING);
        (*cmd).resource_id = 1;
        (*cmd).nr_entries = num_pages as u32;

        // Write scatter-gather entries after the header
        let entries_ptr = cmd_virt.add(core::mem::size_of::<GpuResourceAttachBacking>())
            as *mut GpuMemEntry;
        for (i, &page_phys) in pages.iter().enumerate() {
            let remaining = fb_size.saturating_sub(i * 4096);
            (*entries_ptr.add(i)) = GpuMemEntry {
                addr: page_phys as u64,
                length: remaining.min(4096) as u32,
                padding: 0,
            };
        }

        // Response after command + entries
        let resp_off = core::mem::size_of::<GpuResourceAttachBacking>() + entries_size;
        core::ptr::write_bytes(cmd_virt.add(resp_off), 0, 24);
    }

    let resp_off = core::mem::size_of::<GpuResourceAttachBacking>() + entries_size;
    submit_and_wait(state, cmd_phys, resp_off, cmd_phys + resp_off, 24)?;

    state.fb_phys_pages = pages;
    Ok(())
}

pub(super) fn set_scanout(state: &mut GpuState) -> Result<(), &'static str> {
    let cmd_phys = physical::alloc_page().ok_or("alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuSetScanout;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_SET_SCANOUT);
        (*cmd).r = GpuRect { x: 0, y: 0, width: state.width, height: state.height };
        (*cmd).scanout_id = 0;
        (*cmd).resource_id = 1;

        let resp_off = core::mem::size_of::<GpuSetScanout>();
        core::ptr::write_bytes(cmd_virt.add(resp_off), 0, 24);
    }

    submit_and_wait(state, cmd_phys,
        core::mem::size_of::<GpuSetScanout>(),
        cmd_phys + core::mem::size_of::<GpuSetScanout>(), 24)
}

// ── Zero-copy blob path ────────────────────────────────────────────────
//
// Replaces the legacy CREATE_2D + ATTACH_BACKING + (per-frame
// TRANSFER_TO_HOST_2D) sequence with a single CREATE_BLOB that hands the
// host a scatter-gather list of guest pages it can read directly. After
// SET_SCANOUT_BLOB the screen samples those pages every flush — no copy.
//
// We use BLOB_MEM_GUEST: pages stay owned by the guest, no host-mapped
// userspace pointer round-trips. USE_MAPPABLE is *not* set — that flag is
// for blobs the *guest* wants to see in its own address space (e.g. for
// shared GL surfaces); our case is the reverse, the host reads, the guest
// writes via its kernel mapping.

/// Allocate framebuffer pages and create a blob resource backed by them.
/// Sets `state.fb_phys_pages` on success. Resource id 1 (matches legacy
/// path so any later code that hardcodes id 1 keeps working).
pub(super) fn create_framebuffer_blob(state: &mut GpuState) -> Result<(), &'static str> {
    let fb_size = (state.width * state.height * 4) as usize;
    let num_pages = (fb_size + 4095) / 4096;

    // Allocate physical pages — same scatter-gather pattern as
    // attach_framebuffer_backing. Pages do NOT need to be contiguous;
    // RESOURCE_CREATE_BLOB takes the same GpuMemEntry list shape as
    // ATTACH_BACKING does.
    let mut pages = alloc::vec::Vec::with_capacity(num_pages);
    for _ in 0..num_pages {
        let page = physical::alloc_page().ok_or("FB blob page alloc failed")?;
        let hhdm = crate::memory::paging::hhdm_offset();
        unsafe { core::ptr::write_bytes((hhdm + page) as *mut u8, 0, 4096); }
        pages.push(page);
    }

    crate::serial_str!("[VIRTIO_GPU] Allocated ");
    crate::drivers::serial::write_dec(num_pages as u32);
    crate::serial_str!(" pages for ");
    crate::drivers::serial::write_dec((fb_size / 1024) as u32);
    crate::serial_str!("KB blob framebuffer\n");

    // Header + scatter-gather entries
    let entries_size = num_pages * core::mem::size_of::<GpuMemEntry>();
    let cmd_size = core::mem::size_of::<GpuResourceCreateBlob>() + entries_size;

    // One page is plenty for any framebuffer ≤ ~4MB (256 entries × 16 bytes
    // = 4KB minus header). Modern displays at 1920×1080×4 = 8MB needs 2048
    // pages → 32KB scatter-gather, so we may need multi-page allocation.
    // For the typical 1024×768×4 = 3MB / 768 pages → 12KB SG, also fits.
    // Conservative: bail out if the command wouldn't fit a single page so
    // we don't silently corrupt unrelated memory.
    if cmd_size + 24 > 4096 {
        return Err("blob SG list too large for single command page");
    }

    let cmd_phys = physical::alloc_page().ok_or("blob cmd alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuResourceCreateBlob;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_BLOB);
        (*cmd).resource_id = 1;
        (*cmd).blob_mem = VIRTIO_GPU_BLOB_MEM_GUEST;
        (*cmd).blob_flags = 0;  // no MAPPABLE — host reads, guest doesn't need to mmap
        (*cmd).nr_entries = num_pages as u32;
        (*cmd).blob_id = 0;
        (*cmd).size = fb_size as u64;

        let entries_ptr = cmd_virt.add(core::mem::size_of::<GpuResourceCreateBlob>())
            as *mut GpuMemEntry;
        for (i, &page_phys) in pages.iter().enumerate() {
            let remaining = fb_size.saturating_sub(i * 4096);
            (*entries_ptr.add(i)) = GpuMemEntry {
                addr: page_phys as u64,
                length: remaining.min(4096) as u32,
                padding: 0,
            };
        }

        core::ptr::write_bytes(cmd_virt.add(cmd_size), 0, 24);
    }

    submit_and_wait(state, cmd_phys, cmd_size, cmd_phys + cmd_size, 24)?;

    state.fb_phys_pages = pages;
    Ok(())
}

/// SET_SCANOUT_BLOB — bind the blob resource (id 1) to scanout 0 with
/// the framebuffer geometry. Must run after `create_framebuffer_blob`.
pub(super) fn set_scanout_blob(state: &mut GpuState) -> Result<(), &'static str> {
    let cmd_phys = physical::alloc_page().ok_or("scanout-blob cmd alloc")?;
    let hhdm = crate::memory::paging::hhdm_offset();
    let cmd_virt = (hhdm + cmd_phys) as *mut u8;

    unsafe {
        let cmd = cmd_virt as *mut GpuSetScanoutBlob;
        (*cmd).hdr = make_hdr(VIRTIO_GPU_CMD_SET_SCANOUT_BLOB);
        (*cmd).r = GpuRect { x: 0, y: 0, width: state.width, height: state.height };
        (*cmd).scanout_id = 0;
        (*cmd).resource_id = 1;
        (*cmd).width = state.width;
        (*cmd).height = state.height;
        (*cmd).format = VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM;
        (*cmd).padding = 0;
        (*cmd).strides = [state.width * 4, 0, 0, 0];
        (*cmd).offsets = [0; 4];

        let resp_off = core::mem::size_of::<GpuSetScanoutBlob>();
        core::ptr::write_bytes(cmd_virt.add(resp_off), 0, 24);
    }

    submit_and_wait(state, cmd_phys,
        core::mem::size_of::<GpuSetScanoutBlob>(),
        cmd_phys + core::mem::size_of::<GpuSetScanoutBlob>(), 24)
}
