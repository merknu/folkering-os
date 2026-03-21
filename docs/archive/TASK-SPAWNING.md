# Task Spawning Implementation

**Date**: 2026-01-23
**Status**: IMPLEMENTED ✅
**Build**: SUCCESS (69 KB)

## Overview

Implemented proper task spawning infrastructure to replace direct `jump_to_usermode()` calls. The kernel now creates user tasks through the task management system and uses the scheduler for execution.

## Changes Made

### 1. New Function: `spawn_raw()` (`src/task/spawn.rs`)

Created a simplified task spawning function that bypasses ELF parsing for raw binaries:

```rust
pub fn spawn_raw(code: &[u8], entry_offset: u64)
    -> Result<TaskId, SpawnError>
```

**Purpose**: Spawn tasks from raw executable code without ELF wrapping

**Process**:
1. Allocate new task ID
2. Map code into user address space (0x400000)
3. Allocate user stack (16 KB at 0x7FFFFFFEF000)
4. Create task structure with proper context
5. Update context with correct stack pointers
6. Insert task into global task table
7. Add task to scheduler runqueue

**Benefits**:
- Works with raw assembly test programs
- Simpler than full ELF parsing
- Suitable for bootstrapping and testing
- Foundation for future ELF support

### 2. Scheduler Start Implementation (`src/task/scheduler.rs`)

Implemented actual context switching in `scheduler_start()`:

**Before**:
```rust
pub fn start() -> ! {
    loop {
        if let Some(task_id) = schedule_next() {
            // TODO: Context switch to task_id
            serial_println!("[SCHED] Would switch to task {}", task_id);
        }
        x86_64::instructions::hlt();
    }
}
```

**After**:
```rust
pub fn start() -> ! {
    serial_println!("[SCHED] Scheduler started");
    x86_64::instructions::interrupts::disable();

    loop {
        if let Some(task_id) = schedule_next() {
            serial_println!("[SCHED] Switching to task {}", task_id);
            unsafe { super::switch::switch_to(task_id); }
            serial_println!("[SCHED] Task {} yielded", task_id);
        } else {
            serial_println!("[SCHED] No runnable tasks, halting");
            x86_64::instructions::interrupts::enable();
            x86_64::instructions::hlt();
            x86_64::instructions::interrupts::disable();
        }
    }
}
```

**Key Changes**:
- Actually calls `switch_to()` for context switches
- Disables interrupts during scheduling decisions
- Enables interrupts only when halting (no tasks)
- Provides debug output for tracking execution

### 3. Kernel Initialization Update (`src/lib.rs`)

Replaced direct user-mode jump with task spawning:

**Before**:
```rust
let entry_point = arch::x86_64::usermode::map_and_load_user_code(user_code);
let user_stack = arch::x86_64::usermode::allocate_user_stack();
arch::x86_64::usermode::jump_to_usermode(entry_point, user_stack);
```

**After**:
```rust
match task::spawn_raw(user_code, 0) {
    Ok(task_id) => {
        serial_println!("[BOOT] User task spawned (id={})", task_id);
    }
    Err(e) => {
        serial_println!("[BOOT] Failed to spawn: {:?}", e);
        loop { hlt(); }
    }
}

serial_println!("[BOOT] Starting scheduler...");
task::scheduler_start(); // Does not return
```

**Flow Change**:
- Old: Direct privilege transition (never returns)
- New: Task created, scheduler manages execution

### 4. Module Exports Update (`src/task/mod.rs`)

Added new exports for task spawning:

```rust
pub use spawn::{spawn, spawn_raw, SpawnError};
pub use scheduler::{enqueue, yield_cpu};
```

## Architecture

### Task Creation Flow

```
1. spawn_raw(code, offset)
2. ↓ Allocate task ID (from global counter)
3. ↓ Map code to user space (0x400000)
4. ↓ Allocate user stack (0x7FFFFFFEF000)
5. ↓ Create Task structure
6. ↓ Set context (entry point, stack, user segments)
7. ↓ Insert into global task table (BTreeMap)
8. ↓ Add to scheduler runqueue (VecDeque)
9. → Task ready to run
```

### Scheduler Execution Flow

```
1. scheduler_start()
2. ↓ Disable interrupts
3. → LOOP:
4.   ↓ schedule_next() → Get next task ID
5.   ↓ switch_to(task_id)
6.     ↓ Save current task context
7.     ↓ Switch page table (CR3)
8.     ↓ Restore new task context
9.     → Task executes (in user mode)
10.    ← Task yields (syscall or interrupt)
11.  ← Return to scheduler
12. → LOOP continues
```

### Context Switch Details

When `switch_to(task_id)` is called:

1. **Save Current Context**:
   - All GPRs (rax, rbx, rcx, rdx, rsi, rdi, r8-r15)
   - Stack pointers (rsp, rbp)
   - Instruction pointer (rip from return address)
   - RFLAGS
   - Segment registers (cs, ss)

2. **Switch Address Space**:
   - Read target task's page table
   - Write CR3 register (if different from current)
   - TLB automatically flushed

3. **Restore New Context**:
   - All GPRs from saved context
   - Stack pointers
   - RFLAGS
   - Jump to saved RIP (ret instruction)

4. **Execution Continues**:
   - Task resumes where it left off
   - For new tasks: starts at entry point

## Current Task List

After initialization, two tasks exist:

| ID | Name | State | Entry Point | Stack | Credentials |
|----|------|-------|-------------|-------|-------------|
| 1 | Kernel | Running | 0x0 (dummy) | 0x0 (dummy) | uid=0, System |
| 2 | User Test | Runnable | 0x400000 | 0x7FFFFFFEF000 | uid=0, Untrusted |

**Note**: Kernel task has dummy values because it represents the kernel initialization context, not a real executable.

## Expected Boot Sequence

```
[BOOT] ✅ Phase 3 COMPLETE - IPC & Task system operational

[BOOT] Spawning user-mode test task...

[SPAWN] Created user task 2 at entry=0x400000 stack=0x7ffffffef000
[BOOT] User task spawned successfully (id=2)

[BOOT] Starting scheduler...

[SCHED] Scheduler started, entering task execution loop
[SCHED] Switching to task 2
[SCHED] Task 2 yielded
[SCHED] Switching to task 2
[SCHED] Task 2 yielded
...
```

## Build Results

### Compilation

```bash
cargo build --target x86_64-folkering.json --release
```

**Result**: ✅ SUCCESS
- **Binary Size**: 69 KB (+4 KB from previous)
- **Warnings**: ~30 (unused imports, Rust 2024 compat)
- **Errors**: 0

### Binary Growth

| Version | Size | Change | Feature |
|---------|------|--------|---------|
| Phase 2 | 61 KB | - | User-mode infrastructure |
| Phase 3 Init | 65 KB | +4 KB | IPC & Task initialization |
| Phase 3 Spawn | 69 KB | +4 KB | Task spawning & scheduling |

## Known Limitations

### 1. Shared Page Table

**Issue**: All tasks currently share the kernel page table

**Impact**:
- No memory isolation between tasks
- Tasks can access each other's memory
- Security vulnerability

**Solution** (Future):
- Implement per-task page table creation
- Copy kernel mappings to new page table
- Map only task-specific user pages

### 2. No ELF Support

**Issue**: Can only spawn raw binary code

**Impact**:
- Cannot load standard executables
- No dynamic linking
- Limited to simple test programs

**Solution** (Future):
- Implement ELF parser
- Load program segments
- Handle relocations and dynamic linking

### 3. Cooperative Multitasking Only

**Issue**: Tasks must explicitly yield CPU

**Impact**:
- Misbehaving task can monopolize CPU
- No preemption
- Poor responsiveness

**Solution** (Future):
- Implement timer interrupt
- Preemptive scheduling
- Time slicing (e.g., 10ms per task)

### 4. No Capability Enforcement

**Issue**: Capability checks are stubbed out

**Impact**:
- Tasks can IPC with anyone
- No access control
- Security bypassed

**Solution** (Future):
- Implement capability table
- Grant capabilities explicitly
- Enforce checks in IPC/syscalls

## Testing Status

### Compilation: ✅ PASS

- Kernel builds successfully
- No compilation errors
- All modules link correctly

### Boot Testing: ⏳ PENDING

- QEMU not available in current environment
- Cannot verify runtime behavior
- Next session: Boot test and debug

### Expected Issues

When boot testing becomes available, likely issues:

1. **Task Won't Start**:
   - Context not initialized properly
   - Entry point incorrect
   - Stack alignment issues

2. **Immediate Crash**:
   - Page fault (bad memory access)
   - General protection fault (privilege violation)
   - Triple fault (kernel panic loop)

3. **Scheduler Hangs**:
   - Task never yields
   - Context switch doesn't return
   - Infinite loop in scheduler

## Next Steps

### Immediate (Next Session)

1. **Boot Test**:
   - Run in QEMU
   - Observe scheduler output
   - Verify task switching

2. **Debug Issues**:
   - Fix context initialization
   - Correct stack setup
   - Resolve any crashes

3. **Add Logging**:
   - Track context switches
   - Log syscall invocations
   - Monitor task states

### Short Term

1. **IPC Testing**:
   - Create second user task
   - Send message between tasks
   - Verify reply mechanism

2. **Performance**:
   - Measure context switch time
   - Benchmark IPC latency
   - Compare against targets

3. **Cleanup**:
   - Remove unused code
   - Fix compiler warnings
   - Improve error handling

### Long Term

1. **Per-Task Page Tables**:
   - Memory isolation
   - Security improvement
   - Foundation for MMU features

2. **Preemptive Scheduling**:
   - Timer interrupt handler
   - Time slicing
   - Fair CPU distribution

3. **ELF Loading**:
   - Standard executable support
   - Dynamic linking
   - Shared libraries

## Technical Details

### Memory Layout Per Task

```
User Space:
0x0000_0000_0040_0000: Code (.text)     - 4 KB typical
0x7FFF_FFEF_0000:      Stack base       - 16 KB
0x7FFF_FFF0_0000:      Stack top        - grows down

Kernel Space (shared):
0xFFFF_8000_0000_0000: HHDM             - 128 TB
0xFFFF_FFFF_8000_0000: Kernel code      - 16 MB
0xFFFF_FFFF_8100_0000: Kernel heap      - 16 MB
```

### Context Structure (160 bytes)

```rust
pub struct Context {
    pub rsp: u64,      // 0
    pub rbp: u64,      // 8
    pub rax: u64,      // 16
    pub rbx: u64,      // 24
    pub rcx: u64,      // 32
    pub rdx: u64,      // 40
    pub rsi: u64,      // 48
    pub rdi: u64,      // 56
    pub r8: u64,       // 64
    pub r9: u64,       // 72
    pub r10: u64,      // 80
    pub r11: u64,      // 88
    pub r12: u64,      // 96
    pub r13: u64,      // 104
    pub r14: u64,      // 112
    pub r15: u64,      // 120
    pub rip: u64,      // 128
    pub rflags: u64,   // 136
    pub cs: u64,       // 144
    pub ss: u64,       // 152
}
```

### Task Structure Size

```
Task fields:
- id: u32                    = 4 bytes
- state: TaskState           = 1 byte
- page_table: PageTable      = 4096 bytes
- context: Context           = 160 bytes
- recv_queue: MessageQueue   = ~4 KB (64 msgs × 64 bytes)
- ipc_reply: Option<...>     = 72 bytes
- blocked_on: Option<TaskId> = 8 bytes
- capabilities: Vec<u32>     = 24 bytes (+ heap)
- credentials: Credentials   = 12 bytes

Total (approx): ~8.3 KB per task
```

## References

- **Implementation**: `src/task/spawn.rs`, `src/task/scheduler.rs`
- **Context Switch**: `src/task/switch.rs`
- **Previous Phase**: `docs/PHASE-3-INIT.md`
- **Architecture**: Obsidian vault `output/architecture/`

---

**Session**: 2026-01-23 Task Spawning
**Performed By**: Claude Sonnet 4.5
**Status**: Build complete, boot testing pending
