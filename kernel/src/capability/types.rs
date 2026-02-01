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

            // Memory capability: check if region is covered
            (CapabilityType::Memory { base_page: b1, num_pages: n1, writable: w1 },
             CapabilityType::Memory { base_page: b2, num_pages: n2, writable: w2 }) => {
                // Held capability must cover the required region
                *b2 >= *b1
                    && (*b2 + *n2 as u64) <= (*b1 + *n1 as u64)
                    && (*w1 || !*w2) // If required is writable, held must be too
            }

            // Framebuffer capability: check if address range is covered
            (CapabilityType::Framebuffer { phys_base: b1, size: s1 },
             CapabilityType::Framebuffer { phys_base: b2, size: s2 }) => {
                // Held capability must cover the required range
                *b2 >= *b1 && (*b2 + *s2) <= (*b1 + *s1)
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
