//! Capability Types

/// Capability (128-bit unforgeable token)
#[derive(Clone, Copy, Debug)]
pub struct Capability {
    pub id: u128,
    pub cap_type: CapabilityType,
}

/// Capability types
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapabilityType {
    /// All capabilities (for init process)
    All,
    /// IPC send capability
    IpcSend(u32),
    /// IPC receive capability
    IpcReceive(u32),
    /// File read capability
    FileRead(u64),
    /// File write capability
    FileWrite(u64),
    /// Memory map capability
    MemoryMap,
    /// Scheduler capability
    Scheduler,
}
