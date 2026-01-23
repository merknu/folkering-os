//! Task Spawning
//!
//! Creates new tasks from ELF binaries.
//! Unlike Unix fork/exec, we only have spawn() - create a new task from a binary.

use super::task::{Task, allocate_task_id, insert_task};
use super::elf::{ElfBinary, ElfError};
use super::TaskId;
use crate::memory::{PageTable, paging};
use alloc::vec::Vec;

/// Task spawn error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// Invalid ELF binary
    InvalidElf(ElfError),
    /// Out of memory
    OutOfMemory,
    /// Permission denied
    PermissionDenied,
    /// Binary not found
    NotFound,
}

impl From<ElfError> for SpawnError {
    fn from(err: ElfError) -> Self {
        SpawnError::InvalidElf(err)
    }
}

/// Spawn a new task from an ELF binary
///
/// # Arguments
/// * `binary` - ELF binary data
/// * `args` - Command-line arguments (future enhancement)
///
/// # Returns
/// TaskId of the newly created task, or SpawnError
///
/// # Design Notes
/// - NO fork/exec - only spawn() creates new tasks
/// - Each task gets a fresh page table
/// - Capabilities are explicitly granted (no inheritance by default)
/// - Entry point is read from ELF header
///
/// # TODO
/// - Implement ELF parser
/// - Set up user stack
/// - Load binary segments into memory
/// - Grant initial capabilities
pub fn spawn(binary: &[u8], _args: &[&str]) -> Result<TaskId, SpawnError> {
    // 1. Allocate new task ID
    let task_id = allocate_task_id();

    // 2. Parse ELF binary (stub for now)
    let entry_point = parse_elf(binary)?;

    // 3. Create new page table (stub - reuse kernel page table for now)
    // TODO: Create proper per-task page table
    let page_table = create_task_page_table()?;

    // 4. Create task structure
    let task = Task::new(task_id, page_table, entry_point);

    // 5. Insert into global task table
    insert_task(task);

    // 6. Add to scheduler runqueue
    crate::task::scheduler::enqueue(task_id);

    Ok(task_id)
}

/// Parse ELF binary and return entry point
///
/// Validates ELF binary and extracts entry point address.
fn parse_elf(binary: &[u8]) -> Result<u64, SpawnError> {
    let elf = ElfBinary::parse(binary)?;
    Ok(elf.entry_point())
}

/// Create a new page table for a task
///
/// # TODO
/// - Copy kernel mappings to new page table
/// - Map user stack
/// - Set up higher-half kernel mapping
fn create_task_page_table() -> Result<PageTable, SpawnError> {
    // Stub: Create empty page table
    // TODO: Properly initialize per-task page table

    // For now, return an error since we don't have proper page table creation yet
    Err(SpawnError::OutOfMemory)
}

/// Load ELF segments into task's address space
///
/// # Arguments
/// * `page_table` - Task's page table
/// * `segments` - ELF program segments to load
///
/// # TODO
/// - Allocate physical pages for each segment
/// - Map pages into task's address space
/// - Copy segment data from ELF binary
/// - Set appropriate permissions (R/W/X)
fn load_segments(_page_table: &mut PageTable, _segments: &[ElfSegment]) -> Result<(), SpawnError> {
    // TODO: Implement segment loading
    Ok(())
}

/// ELF program segment (stub)
struct ElfSegment {
    _virt_addr: u64,
    _size: usize,
    _data: Vec<u8>,
    _flags: u32,
}
