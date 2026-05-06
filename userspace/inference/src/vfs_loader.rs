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

/// Reserved virtual address for the inference task's KEEP-MAPPED
/// model file (D.3.7.virtio). The model disk's payload sits here
/// for the lifetime of the process — the shmem is mapped once,
/// never unmapped, and `FbinView` borrows directly into it.
/// Intentionally far from `VFS_VADDR` so both can coexist when a
/// short-lived Synapse read happens during model-loaded steady state.
// 2 MiB-aligned. The kernel's shmem layer now backs large allocations
// (≥ 2 MiB) with 2 MiB huge pages so the 604 MiB Qwen3 weight stream
// collapses from 154,729 4 KiB PTEs to 302 PD entries — fits in dTLB,
// kills TLB-thrash on the inner matmul loop. shmem_map enforces the
// alignment match: 0x6004_0000 (the prior value) was 256 KiB-aligned
// only, which would have rejected the huge mapping.
const MODEL_VADDR: usize = 0x6000_0000;

/// Read a file from Synapse VFS into a freshly allocated `Vec<u8>`.
/// Maps the shmem, copies the bytes out, unmaps, destroys the shmem.
/// The caller owns the Vec and can pass `&[]` slices of it to
/// downstream parsers (e.g. `FbinView::parse`).
///
/// Falls through to the model-disk path (D.3.7.virtio) when
/// Synapse reports `NotFound` — same `ShmemFileResponse` shape, so
/// the rest of the function is shared. A `qwen.fbin` packed in
/// initrd takes the Synapse path; a `qwen.fbin` on the secondary
/// virtio_blk takes the model-disk syscall path. Inference code
/// never needs to know which.
///
/// **Caveat:** copies the file into a Vec, so files larger than
/// the bump heap (64 MiB) will OOM. Use `read_file_mapped` for
/// large model files — it borrows the shmem directly and never
/// copies.
pub fn read_file(name: &str) -> Result<Vec<u8>, VfsError> {
    let resp = match synapse::read_file_shmem(name) {
        Ok(r) => r,
        Err(SynapseError::NotFound) => {
            // Try the model disk before declaring NotFound. Cheap
            // when no model disk is attached (kernel returns u64::MAX
            // immediately).
            match synapse::read_model_file_shmem(name) {
                Ok(r) => r,
                Err(SynapseError::NotFound) => {
                    // Last resort: read directly from initrd ramdisk.
                    // Synapse-with-SQLite-backend ignores FPK entries,
                    // so files like qwen.tokb that ship in initrd but
                    // aren't registered in the SQLite `files` table
                    // would otherwise be unreachable. The kernel
                    // ramdisk syscall (SYS_FS_READ_FILE = 14) walks
                    // FPK entries directly.
                    // Kernel SYS_FS_READ_FILE caps buf_size at 4 MiB
                    // (`buf_size > 4 * 1024 * 1024 → u64::MAX`); the only
                    // file we currently fall back for is qwen.tokb at
                    // ~3.79 MiB, so this fits with margin.
                    let mut buf = alloc::vec![0u8; 4 * 1024 * 1024];
                    let n = libfolk::sys::fs::read_file(name, &mut buf);
                    if n == 0 {
                        return Err(VfsError::NotFound);
                    }
                    buf.truncate(n);
                    return Ok(buf);
                }
                Err(e) => return Err(VfsError::Synapse(e)),
            }
        },
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

/// Read a file via keep-mapped shmem and return a `&'static [u8]`
/// pointing into the mapping. Zero-copy — the file's bytes live
/// in shmem-backed pages mapped once at `MODEL_VADDR` and never
/// unmapped. Required for files larger than the bump heap (qwen.fbin
/// at 232 MiB exceeds our 64 MiB heap by 4×, so `read_file`'s
/// Vec-copy would OOM).
///
/// The lifetime is fictional — we promise the caller never to
/// unmap. If the inference task ever needs to reload the model
/// from a different disk, this function would need to grow an
/// `unmap_model()` companion. Today there's only one model and it
/// lives here forever.
///
/// Synapse-first / model-disk-fallback ordering matches `read_file`.
pub fn read_file_mapped(name: &str) -> Result<&'static [u8], VfsError> {
    let resp = match synapse::read_file_shmem(name) {
        Ok(r) => r,
        Err(SynapseError::NotFound) => {
            match synapse::read_model_file_shmem(name) {
                Ok(r) => r,
                Err(SynapseError::NotFound) => return Err(VfsError::NotFound),
                Err(e) => return Err(VfsError::Synapse(e)),
            }
        },
        Err(e) => return Err(VfsError::Synapse(e)),
    };
    if resp.size == 0 {
        let _ = shmem_destroy(resp.shmem_handle);
        return Err(VfsError::Synapse(SynapseError::IpcFailed));
    }
    if shmem_map(resp.shmem_handle, MODEL_VADDR).is_err() {
        let _ = shmem_destroy(resp.shmem_handle);
        return Err(VfsError::ShmemMap);
    }
    // SAFETY: shmem_map succeeded for `resp.size` bytes at
    // MODEL_VADDR. We deliberately don't unmap or destroy — the
    // mapping persists for the process lifetime so the `&'static`
    // lifetime is honest. Re-calling this for the same name would
    // map a SECOND shmem at the same vaddr (kernel rejects),
    // returning ShmemMap. Caller is expected to call once.
    let bytes: &'static [u8] = unsafe {
        core::slice::from_raw_parts(MODEL_VADDR as *const u8, resp.size as usize)
    };
    Ok(bytes)
}
