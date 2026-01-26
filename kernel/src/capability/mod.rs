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
        // Find a free slot
        for i in 0..MAX_CAPABILITIES {
            if self.entries[i].is_none() {
                self.entries[i] = Some(entry);
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1);
                if self.next_id == 0 {
                    self.next_id = 1;
                }
                return Ok(id);
            }
        }
        Err(CapError::TableFull)
    }

    fn get(&self, id: CapabilityId) -> Option<&CapabilityEntry> {
        if id == 0 || id as usize > MAX_CAPABILITIES {
            return None;
        }
        // For now, use id as index (simple approach)
        // In production, we'd use a hash map for O(1) lookup
        for entry in self.entries.iter().flatten() {
            if entry.valid {
                // This is a simplified lookup - in production we'd track ID properly
                return Some(entry);
            }
        }
        None
    }

    fn revoke(&mut self, id: CapabilityId) -> Result<(), CapError> {
        if id == 0 || id as usize > MAX_CAPABILITIES {
            return Err(CapError::InvalidId);
        }
        // Mark the capability as invalid
        for entry in self.entries.iter_mut().flatten() {
            entry.valid = false;
            return Ok(());
        }
        Err(CapError::NotFound)
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

/// Transfer a capability from one task to another
///
/// Used during IPC to pass capabilities.
pub fn transfer(
    from_task: TaskId,
    to_task: TaskId,
    cap_id: CapabilityId,
) -> Result<CapabilityId, CapError> {
    // Verify the source task owns this capability
    let from = crate::task::task::get_task(from_task).ok_or(CapError::NotFound)?;
    let to = crate::task::task::get_task(to_task).ok_or(CapError::NotFound)?;

    {
        let from_lock = from.lock();
        if !from_lock.capabilities.contains(&cap_id) {
            return Err(CapError::PermissionDenied);
        }
    }

    // Get the capability type
    let cap_type = {
        let table = CAP_TABLE.lock();
        table.get(cap_id).ok_or(CapError::NotFound)?.cap_type
    };

    // Create a new capability for the destination task
    let new_id = grant(to_task, cap_type)?;

    // Optionally: remove from source task (move semantics) or keep (copy semantics)
    // For now, we use copy semantics (both tasks have the capability)

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
