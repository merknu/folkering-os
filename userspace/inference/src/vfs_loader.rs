//! Phase D.3.1.2 — Synapse VFS file reader.
//!
//! Wraps `libfolk::sys::synapse::read_file_shmem` + `shmem_map`/
//! `shmem_unmap` so the inference task can pull `.fbin` files out
//! of Synapse with one call. Today the file gets into VFS via
//! folk-pack's `--add NAME:data:PATH` option (the build packs
//! `boot/iso_root/model_test.fbin` into the ramdisk; Synapse's
//! `refresh_fpk_cache` exposes it by name at boot).
//!
//! D.3.1.3 will swap the test fbin for a real Qwen2.5-0.5B fbin
//! produced by the HuggingFace converter; this module's API stays
//! identical.

extern crate alloc;

use alloc::vec::Vec;

use libfolk::sys::synapse::{self, SynapseError};
use libfolk::sys::{shmem_map, shmem_unmap, shmem_destroy};

/// Reserved virtual address for the inference task's VFS read
/// mappings. We read one file at a time during the boot self-test,
/// then unmap, so a single slot suffices. If a future use case
/// streams multiple files concurrently it gets its own vaddr.
///
/// Address picked in the same low-half range the compositor uses
/// for its `VFS_OPEN_VADDR` (`0x50040000`); Synapse's shmem grant
/// path expects mapping requests inside the standard user range
/// rather than the per-task upper-half private zone our IPC
/// request buffer (`router::REQ_VADDR = 0x4100_0000_0000`) lives
/// in. The upper-half mappings work for shmem we *create* (e.g.
/// the compositor display-list rings); shmem we *receive* via
/// Synapse needs the lower half for the kernel's grant fast-path.
const VFS_VADDR: usize = 0x5004_0000;

#[derive(Debug)]
#[allow(dead_code)] // payload fields read via Debug only
pub enum VfsError {
    NotFound,
    Synapse(SynapseError),
    ShmemMap,
}

/// Read a file from Synapse VFS into a freshly allocated `Vec<u8>`.
/// Maps the shmem, copies the bytes out, unmaps, destroys the shmem.
/// The caller owns the Vec and can pass `&[]` slices of it to
/// downstream parsers (e.g. `FbinView::parse`).
///
/// We could keep the mapping live and hand back a `&'static [u8]`
/// for zero-copy access, but the bump allocator + the simplicity of
/// "the file lives in the heap from now on" wins for the small
/// test files D.3.1.2 deals with. When real models land (~250 MiB
/// quantized), we'll switch to a keep-mapped variant — the API
/// will need to grow to expose the lifetime.
pub fn read_file(name: &str) -> Result<Vec<u8>, VfsError> {
    let resp = match synapse::read_file_shmem(name) {
        Ok(r) => r,
        Err(SynapseError::NotFound) => return Err(VfsError::NotFound),
        Err(e) => return Err(VfsError::Synapse(e)),
    };
    if resp.size == 0 {
        // Synapse can return an empty file legitimately, but for
        // the inference path we treat it as a content error rather
        // than letting the parser swallow it as "no tensors".
        let _ = shmem_destroy(resp.shmem_handle);
        return Err(VfsError::Synapse(SynapseError::IpcFailed));
    }
    if shmem_map(resp.shmem_handle, VFS_VADDR).is_err() {
        let _ = shmem_destroy(resp.shmem_handle);
        return Err(VfsError::ShmemMap);
    }
    // SAFETY: we just successfully mapped `resp.size` bytes at
    // VFS_VADDR. The slice is valid until the unmap below.
    let bytes = unsafe {
        core::slice::from_raw_parts(VFS_VADDR as *const u8, resp.size as usize)
    };
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(bytes);
    let _ = shmem_unmap(resp.shmem_handle, VFS_VADDR);
    let _ = shmem_destroy(resp.shmem_handle);
    Ok(out)
}
