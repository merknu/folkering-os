//! Capability Types
//!
//! Unforgeable capability tokens for access control in the microkernel.
//! Capabilities are the basis of all security in Folkering OS.

use crate::task::TaskId;

/// Capability ID (index into global capability table)
pub type CapabilityId = u32;

/// Capability entry in the global table
#[derive(Clone, Copy, Debug)]
pub struct CapabilityEntry {
    /// The type and rights this capability grants
    pub cap_type: CapabilityType,
    /// Owner task ID (who holds this capability)
    pub owner: TaskId,
    /// Reference count (how many tasks hold a copy)
    pub ref_count: u32,
    /// Is this capability valid? (false = revoked)
    pub valid: bool,
}

impl CapabilityEntry {
    pub const fn new(cap_type: CapabilityType, owner: TaskId) -> Self {
        Self {
            cap_type,
            owner,
            ref_count: 1,
            valid: true,
        }
    }
}

/// Capability types - defines what rights a capability grants
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityType {
    /// All capabilities (for init/kernel processes only)
    All,

    /// IPC send capability to a specific task
    IpcSend(TaskId),

    /// IPC send capability to any task (for services)
    IpcSendAny,

    /// IPC receive capability (implicit for all tasks)
    IpcReceive,

    /// Memory region capability (for shared memory)
    Memory {
        base_page: u64,
        num_pages: u32,
        writable: bool,
    },

    /// File/resource handle capability
    Resource(u64),

    /// Task control capability (spawn, kill children)
    TaskControl,

    /// Scheduler control capability (change priorities)
    Scheduler,

    /// Hardware access capability (for drivers)
    Hardware(u32), // Device ID

    /// Framebuffer access capability
    /// Contains the physical address range allowed
    Framebuffer {
        phys_base: u64,
        size: u64,
    },

    /// DMA-buffer ownership capability.
    ///
    /// Auto-granted to the caller of `syscall_dma_alloc` for the
    /// physical range it just reserved. Held regions are the only
    /// physical addresses `syscall_dma_sync_read` / `_write` will
    /// let the task operate on — without this gate a compromised
    /// task could sync-write arbitrary kernel memory via HHDM.
    DmaRegion {
        phys_base: u64,
        size: u64,
    },

    /// PCI MMIO BAR mapping capability.
    ///
    /// Granted to the compositor at boot for every decoded MMIO BAR
    /// in the PCI device table. `syscall_map_physical` checks this
    /// before letting a task map a hardware register range — closes
    /// the old `is_pci_mmio = phys >= 0xF000_0000` bypass that let
    /// any task reprogram any VirtIO/GPU/NIC device.
    ///
    /// Unlike `DmaRegion`, these ranges are hardware, not PMM-owned:
    /// `revoke_with_cleanup` must NOT call `free_pages` on them.
    MmioRegion {
        phys_base: u64,
        size: u64,
    },

    /// PCI I/O-port BAR access capability.
    ///
    /// Covers a range `[base, base + size)` of x86 I/O ports. Used by
    /// `syscall_port_{in,out}{b,w,l}` to authorize the requesting
    /// task. Prior to this variant, port I/O was gated by a global
    /// `port_io_allowed()` that walked `PCI_DEVICES` and allowed any
    /// task to touch any PCI device's I/O BAR — fine when there's
    /// only one userspace driver, but a sandbox-escape in principle.
    IoPort {
        base: u16,
        size: u16,
    },

    /// Authorizes a task to call `syscall_pci_acquire` — i.e. to
    /// become a userspace device driver. Gates the grant-yourself-
    /// MMIO-caps loophole: without this, any task could acquire any
    /// PCI device and bypass the "only compositor touches hardware"
    /// invariant. Granted to the compositor at boot so the blanket
    /// MMIO grant can be augmented with per-device additions later;
    /// future driver-privileged tasks would need explicit grants.
    DriverPrivilege,

    /// Authorizes raw VirtIO block-device syscalls
    /// (`syscall_block_read` / `syscall_block_write`). Without this,
    /// any task can read/write arbitrary disk sectors — including
    /// Synapse's SQLite DB, the MVFS region, the GGUF model payload,
    /// and the FOLKDISK boot header. Granted only to Synapse (which
    /// legitimately needs raw block I/O) at task spawn; everyone
    /// else goes through Synapse IPC or MVFS syscalls for storage.
    RawBlockIO,
}

/// Well-known device IDs for Hardware capabilities
pub mod device_ids {
    /// Framebuffer device ID
    pub const FRAMEBUFFER: u32 = 0x0001;
    /// Keyboard device ID
    pub const KEYBOARD: u32 = 0x0002;
    /// Serial port device ID
    pub const SERIAL: u32 = 0x0003;
}

impl CapabilityType {
    /// Check if this capability grants the required access
    pub fn grants(&self, required: &CapabilityType) -> bool {
        match (self, required) {
            // All grants everything
            (CapabilityType::All, _) => true,

            // Exact match
            (a, b) if a == b => true,

            // IpcSendAny grants IpcSend to any specific task
            (CapabilityType::IpcSendAny, CapabilityType::IpcSend(_)) => true,

            // Memory capability: check if region is covered.
            // Use `checked_add` so a caller claim of
            // `(base=0, num=u64::MAX)` can't wrap to 0 and sneak past
            // a small granted range.
            (CapabilityType::Memory { base_page: b1, num_pages: n1, writable: w1 },
             CapabilityType::Memory { base_page: b2, num_pages: n2, writable: w2 }) => {
                let held_end = b1.checked_add(*n1 as u64);
                let req_end = b2.checked_add(*n2 as u64);
                match (held_end, req_end) {
                    (Some(he), Some(re)) => {
                        *b2 >= *b1
                            && re <= he
                            && (*w1 || !*w2)
                    }
                    _ => false,
                }
            }

            // Framebuffer capability: check if address range is covered.
            // Same overflow defense as above.
            (CapabilityType::Framebuffer { phys_base: b1, size: s1 },
             CapabilityType::Framebuffer { phys_base: b2, size: s2 }) => {
                let held_end = b1.checked_add(*s1);
                let req_end = b2.checked_add(*s2);
                match (held_end, req_end) {
                    (Some(he), Some(re)) => *b2 >= *b1 && re <= he,
                    _ => false,
                }
            }

            // DmaRegion capability: same range-covers-range semantics
            // as Framebuffer. Held region must fully enclose the
            // requested `[phys_base, phys_base + size)`.
            (CapabilityType::DmaRegion { phys_base: b1, size: s1 },
             CapabilityType::DmaRegion { phys_base: b2, size: s2 }) => {
                let held_end = b1.checked_add(*s1);
                let req_end = b2.checked_add(*s2);
                match (held_end, req_end) {
                    (Some(he), Some(re)) => *b2 >= *b1 && re <= he,
                    _ => false,
                }
            }

            // MmioRegion capability: same range-covers-range semantics
            // as Framebuffer/DmaRegion. A task holding a cap for
            // BAR0's 64 KiB is allowed to map any subrange of that.
            (CapabilityType::MmioRegion { phys_base: b1, size: s1 },
             CapabilityType::MmioRegion { phys_base: b2, size: s2 }) => {
                let held_end = b1.checked_add(*s1);
                let req_end = b2.checked_add(*s2);
                match (held_end, req_end) {
                    (Some(he), Some(re)) => *b2 >= *b1 && re <= he,
                    _ => false,
                }
            }

            // IoPort capability: u16-bounded so overflow is
            // impossible, but keep the same pattern for consistency.
            (CapabilityType::IoPort { base: b1, size: s1 },
             CapabilityType::IoPort { base: b2, size: s2 }) => {
                let held_end = (*b1 as u32) + (*s1 as u32);
                let req_end  = (*b2 as u32) + (*s2 as u32);
                *b2 >= *b1 && req_end <= held_end
            }

            _ => false,
        }
    }
}

/// Result of capability operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapError {
    /// Capability not found
    NotFound,
    /// Capability already revoked
    Revoked,
    /// Permission denied (capability doesn't grant this access)
    PermissionDenied,
    /// Capability table full
    TableFull,
    /// Invalid capability ID
    InvalidId,
    /// Cannot transfer this capability
    NonTransferable,
}
