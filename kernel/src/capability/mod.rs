//! Capability System
//!
//! Unforgeable capability tokens for access control.
//! This is the security foundation of the Folkering microkernel.
//!
//! # Design
//! - Global capability table maps CapabilityId -> CapabilityEntry
//! - Each task holds a list of CapabilityIds it owns
//! - Capabilities can be transferred via IPC
//! - Capabilities can be revoked (invalidated)
//!
//! # Usage
//! ```ignore
//! // Check if task can send IPC to target
//! if capability::has_ipc_send(task_id, target_id) {
//!     // Allowed
//! }
//!
//! // Grant a capability to a task
//! let cap_id = capability::grant(task_id, CapabilityType::IpcSend(target_id))?;
//! ```

pub mod types;

pub use types::{CapabilityEntry, CapabilityId, CapabilityType, CapError};

use crate::task::TaskId;
use spin::Mutex;

/// Maximum number of capabilities in the global table
const MAX_CAPABILITIES: usize = 4096;

/// Global capability table
static CAP_TABLE: Mutex<CapabilityTable> = Mutex::new(CapabilityTable::new());

/// Capability table structure
struct CapabilityTable {
    entries: [Option<CapabilityEntry>; MAX_CAPABILITIES],
    next_id: u32,
}

impl CapabilityTable {
    const fn new() -> Self {
        const NONE: Option<CapabilityEntry> = None;
        Self {
            entries: [NONE; MAX_CAPABILITIES],
            next_id: 1, // Start at 1, 0 is reserved for "no capability"
        }
    }

    fn allocate(&mut self, entry: CapabilityEntry) -> Result<CapabilityId, CapError> {
        let id = self.next_id;
        let idx = id as usize;
        if idx >= MAX_CAPABILITIES {
            return Err(CapError::TableFull);
        }
        if self.entries[idx].is_some() {
            // Slot taken — find next free
            for i in 1..MAX_CAPABILITIES {
                if self.entries[i].is_none() {
                    self.entries[i] = Some(entry);
                    self.next_id = (i as u32) + 1;
                    return Ok(i as CapabilityId);
                }
            }
            return Err(CapError::TableFull);
        }
        self.entries[idx] = Some(entry);
        self.next_id = id + 1;
        Ok(id)
    }

    fn get(&self, id: CapabilityId) -> Option<&CapabilityEntry> {
        let idx = id as usize;
        if idx >= MAX_CAPABILITIES {
            return None;
        }
        self.entries[idx].as_ref()
    }

    fn revoke(&mut self, id: CapabilityId) -> Result<(), CapError> {
        let idx = id as usize;
        if idx >= MAX_CAPABILITIES {
            return Err(CapError::InvalidId);
        }
        // Take the slot so the allocator can reuse it. The earlier
        // `valid = false` marker left slots occupied forever, and
        // `allocate` only considers `None` slots reusable — so over
        // many grant/revoke cycles the cap table would slowly fill
        // up and start returning `TableFull` to legitimate callers.
        match self.entries[idx].take() {
            Some(_) => Ok(()),
            None => Err(CapError::NotFound),
        }
    }
}

/// Initialize capability system
pub fn init() {
    crate::drivers::serial::write_str("[CAP] Capability system initialized\n");
    // Table is already initialized via const fn
}

/// Grant a capability to a task
///
/// Creates a new capability entry and adds it to the task's capability list.
pub fn grant(task_id: TaskId, cap_type: CapabilityType) -> Result<CapabilityId, CapError> {
    let entry = CapabilityEntry::new(cap_type, task_id);
    let id = CAP_TABLE.lock().allocate(entry)?;

    // Add capability to task's list
    if let Some(task) = crate::task::task::get_task(task_id) {
        task.lock().capabilities.push(id);
    }

    crate::drivers::serial::write_str("[CAP] Granted cap ");
    crate::drivers::serial::write_dec(id);
    crate::drivers::serial::write_str(" to task ");
    crate::drivers::serial::write_dec(task_id);
    crate::drivers::serial::write_newline();

    Ok(id)
}

/// Check if a task has a specific capability
pub fn has_capability(task_id: TaskId, required: CapabilityType) -> bool {
    let task = match crate::task::task::get_task(task_id) {
        Some(t) => t,
        None => return false,
    };

    let task_lock = task.lock();

    // Check each capability the task holds
    for &cap_id in &task_lock.capabilities {
        let table = CAP_TABLE.lock();
        if let Some(entry) = table.get(cap_id) {
            if entry.valid && entry.cap_type.grants(&required) {
                return true;
            }
        }
    }

    false
}

/// Check if task can send IPC to target
///
/// This is the primary capability check for IPC operations.
pub fn has_ipc_send(task_id: TaskId, target_id: TaskId) -> bool {
    // Special case: task 0 (kernel) can always send
    if task_id == 0 {
        return true;
    }

    // Check for IpcSend(target) or IpcSendAny capability
    has_capability(task_id, CapabilityType::IpcSend(target_id))
}

/// Grant IPC send capability to a task
pub fn grant_ipc_send(task_id: TaskId, target_id: TaskId) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::IpcSend(target_id))
}

/// Grant IPC send-to-any capability (for services)
pub fn grant_ipc_send_any(task_id: TaskId) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::IpcSendAny)
}

/// Grant all capabilities (for init process)
pub fn grant_all(task_id: TaskId) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::All)
}

/// Revoke a capability
pub fn revoke(cap_id: CapabilityId) -> Result<(), CapError> {
    CAP_TABLE.lock().revoke(cap_id)
}

/// Revoke a capability AND free any backing resources it owned.
///
/// Currently handles `DmaRegion` variants: their underlying
/// `alloc_contiguous`-allocated physical pages are returned to the
/// page allocator. Called from `syscall_exit` so a crashing task
/// doesn't silently leak its DMA buffers.
///
/// CAVEAT — double-free risk if DmaRegion caps are ever transferred:
/// `transfer()` today creates an independent cap entry pointing at
/// the same `(phys_base, size)`, not a refcounted alias. If two
/// tasks hold DmaRegion caps for the same range and both exit, the
/// second exit will free-pages twice. No code path transfers
/// DmaRegion today, but if one ever does, refcount the backing
/// allocation before reusing this helper.
pub fn revoke_with_cleanup(cap_id: CapabilityId) -> Result<(), CapError> {
    // Read the cap type out first so we can act on it after revoke.
    let cap_type = {
        let table = CAP_TABLE.lock();
        table.get(cap_id).map(|e| e.cap_type)
    };

    if let Some(CapabilityType::DmaRegion { phys_base, size }) = cap_type {
        let num_pages = ((size as usize) + 4095) / 4096;
        // Compute the order `alloc_contiguous` used: smallest power
        // of 2 ≥ num_pages. Must match or `free_pages` corrupts the
        // buddy allocator's internal free lists.
        let mut order = 0usize;
        while (1usize << order) < num_pages && order < 10 {
            order += 1;
        }
        crate::memory::physical::free_pages(phys_base as usize, order);
    }

    revoke(cap_id)
}

/// Grant framebuffer capability to a task
pub fn grant_framebuffer(task_id: TaskId, phys_base: u64, size: u64) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::Framebuffer { phys_base, size })
}

/// Check if task has framebuffer capability covering the given range
pub fn has_framebuffer_access(task_id: TaskId, phys_addr: u64, size: u64) -> bool {
    has_capability(task_id, CapabilityType::Framebuffer {
        phys_base: phys_addr,
        size,
    })
}

/// Grant a DMA region capability to a task.
///
/// Called by `syscall_dma_alloc` after it successfully reserves a
/// contiguous physical range for the task. The capability is the
/// task's proof-of-ownership — subsequent `syscall_dma_sync_*` calls
/// check against this range before touching physical memory.
pub fn grant_dma_region(task_id: TaskId, phys_base: u64, size: u64) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::DmaRegion { phys_base, size })
}

/// Check if a task holds a DMA region capability covering the given
/// `[phys_addr, phys_addr + size)` range. Returns false if any slice
/// of the request falls outside every held region.
pub fn has_dma_access(task_id: TaskId, phys_addr: u64, size: u64) -> bool {
    has_capability(task_id, CapabilityType::DmaRegion {
        phys_base: phys_addr,
        size,
    })
}

/// Grant a PCI MMIO region capability to a task. Called from boot
/// after PCI enumeration, one invocation per non-empty MMIO BAR on
/// each enumerated device.
pub fn grant_mmio_region(task_id: TaskId, phys_base: u64, size: u64) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::MmioRegion { phys_base, size })
}

/// Check if a task holds an MMIO region capability covering the
/// given `[phys_addr, phys_addr + size)` range. Used by
/// `syscall_map_physical` to gate PCI BAR mapping.
pub fn has_mmio_access(task_id: TaskId, phys_addr: u64, size: u64) -> bool {
    has_capability(task_id, CapabilityType::MmioRegion {
        phys_base: phys_addr,
        size,
    })
}

/// Grant a PCI I/O-port range capability to a task.
pub fn grant_io_port(task_id: TaskId, base: u16, size: u16) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::IoPort { base, size })
}

/// Check if a task may touch I/O port `[base, base + size)`. Used by
/// every `syscall_port_{in,out}{b,w,l}` entry point.
pub fn has_io_port_access(task_id: TaskId, base: u16, size: u16) -> bool {
    has_capability(task_id, CapabilityType::IoPort { base, size })
}

/// Grant the "authorized device driver" privilege to a task. Without
/// this, `syscall_pci_acquire` is a no-op — a task can't escalate its
/// own hardware access by asking to drive a device.
pub fn grant_driver_privilege(task_id: TaskId) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::DriverPrivilege)
}

/// Check if a task is allowed to call `syscall_pci_acquire`.
pub fn has_driver_privilege(task_id: TaskId) -> bool {
    has_capability(task_id, CapabilityType::DriverPrivilege)
}

/// Grant raw block-device I/O capability. Only Synapse gets this
/// today — it's the canonical owner of the SQLite region and the
/// only task that legitimately needs `syscall_block_read` /
/// `_write` direct sector access.
pub fn grant_raw_block_io(task_id: TaskId) -> Result<CapabilityId, CapError> {
    grant(task_id, CapabilityType::RawBlockIO)
}

/// Check if a task may perform raw block-device I/O.
pub fn has_raw_block_io(task_id: TaskId) -> bool {
    has_capability(task_id, CapabilityType::RawBlockIO)
}

/// Capability variants that MUST NOT be transferred via IPC.
///
/// These caps authorize access to hardware or physical memory that
/// the kernel has direct authority over. Transferring them would let
/// a privileged task bootstrap a less-privileged task past the
/// intended gate. In particular:
///
///   - `DmaRegion`: transferring double-frees if both tasks exit
///     (each `revoke_with_cleanup` frees the same phys range).
///   - `MmioRegion`/`IoPort`: break "only boot-authorized tasks touch
///     hardware" — a kernel-granted cap becomes a userspace token.
///   - `Framebuffer`: same reasoning as MMIO.
///   - `DriverPrivilege`: the gate on `pci_acquire`. Transferring
///     defeats the whole point of gating it.
///   - `RawBlockIO`: Synapse-only disk access.
///
/// IPC-style caps (`IpcSend`, `IpcSendAny`, `IpcReceive`) and
/// general-purpose ones (`Memory`, `TaskControl`, `Scheduler`,
/// `Hardware(_)`, `Resource(_)`, `All`) remain transferable.
///
/// This is a private helper used only by `TransferableCap::check` —
/// external code can't call it to "verify" a cap before bypassing
/// `transfer()`. The only path to transfer is through the newtype.
fn is_non_transferable(cap_type: &CapabilityType) -> bool {
    matches!(
        cap_type,
        CapabilityType::DmaRegion { .. }
            | CapabilityType::MmioRegion { .. }
            | CapabilityType::IoPort { .. }
            | CapabilityType::Framebuffer { .. }
            | CapabilityType::DriverPrivilege
            | CapabilityType::RawBlockIO
    )
}

/// A `CapabilityId` that has been validated as transferable.
///
/// The only way to construct one is via `TransferableCap::check`,
/// which inspects the cap's type and returns `None` for hardware-
/// bound variants. This makes the transferability rule **type-
/// enforced**, not comment-enforced: `transfer()` accepts only
/// `TransferableCap`, so a future regression that forgets to call
/// `is_non_transferable` becomes a compile error, not a runtime
/// escape hatch.
///
/// Use pattern:
/// ```ignore
/// let tc = TransferableCap::check(cap_id)
///     .ok_or(CapError::NonTransferable)?;
/// capability::transfer(from, to, tc)?;
/// ```
#[derive(Clone, Copy, Debug)]
pub struct TransferableCap(CapabilityId);

impl TransferableCap {
    /// Validate that `cap_id` refers to a transferable cap and return
    /// the typed wrapper. Returns `None` if the cap is missing or is
    /// a hardware-bound variant.
    pub fn check(cap_id: CapabilityId) -> Option<Self> {
        let table = CAP_TABLE.lock();
        let entry = table.get(cap_id)?;
        if is_non_transferable(&entry.cap_type) {
            return None;
        }
        Some(TransferableCap(cap_id))
    }

    /// Unwrap to the underlying CapabilityId (e.g. for logging).
    pub fn id(self) -> CapabilityId {
        self.0
    }
}

/// Transfer a capability from one task to another
///
/// Used during IPC to pass capabilities. The `cap` parameter is a
/// `TransferableCap` — the type system prevents hardware-bound caps
/// from reaching this function at all.
pub fn transfer(
    from_task: TaskId,
    to_task: TaskId,
    cap: TransferableCap,
) -> Result<CapabilityId, CapError> {
    let cap_id = cap.id();

    // Verify the source task owns this capability
    let from = crate::task::task::get_task(from_task).ok_or(CapError::NotFound)?;
    let to = crate::task::task::get_task(to_task).ok_or(CapError::NotFound)?;

    {
        let from_lock = from.lock();
        if !from_lock.capabilities.contains(&cap_id) {
            return Err(CapError::PermissionDenied);
        }
    }

    // Get the capability type. TransferableCap guarantees the
    // transferability check already passed — but we re-read the
    // type here because the caller might have held the cap_id
    // across a revoke + re-grant, and we want the CURRENT type.
    let cap_type = {
        let table = CAP_TABLE.lock();
        table.get(cap_id).ok_or(CapError::NotFound)?.cap_type
    };

    // Belt-and-braces: if the cap has somehow mutated into a
    // non-transferable variant between TransferableCap::check and
    // now (shouldn't be possible under current code, but defense in
    // depth), refuse the transfer. This is the only place where a
    // runtime fallback makes sense — the type check got us here.
    if is_non_transferable(&cap_type) {
        return Err(CapError::NonTransferable);
    }

    // Create a new capability for the destination task
    let new_id = grant(to_task, cap_type)?;

    // Copy semantics: both tasks now hold the capability. Move
    // semantics would require revoking the source — left as future
    // work if/when a use case demands it.

    crate::drivers::serial::write_str("[CAP] Transferred cap from task ");
    crate::drivers::serial::write_dec(from_task);
    crate::drivers::serial::write_str(" to task ");
    crate::drivers::serial::write_dec(to_task);
    crate::drivers::serial::write_newline();

    Ok(new_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capability_grants() {
        let all = CapabilityType::All;
        let send_2 = CapabilityType::IpcSend(2);
        let send_any = CapabilityType::IpcSendAny;

        // All grants everything
        assert!(all.grants(&send_2));
        assert!(all.grants(&send_any));

        // SendAny grants specific sends
        assert!(send_any.grants(&send_2));

        // Specific send only grants that target
        assert!(send_2.grants(&send_2));
        assert!(!send_2.grants(&CapabilityType::IpcSend(3)));
    }
}
