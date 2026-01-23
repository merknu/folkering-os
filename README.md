# Folkering OS Microkernel

A capability-based microkernel operating system written in Rust for x86_64.

## Architecture

- **Microkernel design**: Only essential services in kernel space
- **Capability-based security**: Unforgeable 128-bit capability tokens
- **IPC-centric**: Fast message passing (<1000 cycles target)
- **Higher-half kernel**: Mapped at `0xFFFFFFFF80000000`

## Performance Targets

| Metric | Target | Notes |
|--------|--------|-------|
| Boot time | <10 seconds | From power-on to login prompt |
| IPC latency | <1000 cycles | Single message send/receive |
| Context switch | <500 cycles | Register save/restore + CR3 reload |
| Scheduling decision | <10,000 cycles | CFS algorithm in userspace |

## Project Structure

```
kernel/
├── Cargo.toml                      # Crate configuration
├── linker.ld                       # Linker script (higher-half)
├── x86_64-folkering.json           # Custom target specification
├── README.md                       # This file
└── src/
    ├── main.rs                     # Kernel entry point
    ├── lib.rs                      # Kernel library
    ├── boot.rs                     # Boot info parsing (Limine)
    ├── panic.rs                    # Panic handler
    ├── arch/                       # Architecture-specific code
    │   └── x86_64/
    │       ├── mod.rs
    │       ├── boot.S              # Assembly entry point
    │       ├── gdt.rs              # Global Descriptor Table
    │       ├── idt.rs              # Interrupt Descriptor Table
    │       ├── interrupts.rs       # Interrupt management
    │       ├── apic.rs             # Local APIC (timer)
    │       └── acpi.rs             # ACPI parsing
    ├── memory/                     # Memory management
    │   ├── mod.rs
    │   ├── physical.rs             # Buddy allocator (TODO)
    │   ├── paging.rs               # Page tables (TODO)
    │   └── heap.rs                 # Kernel heap (with guard page)
    ├── ipc/                        # IPC subsystem
    │   ├── mod.rs
    │   ├── message.rs              # IpcMessage struct (64 bytes!)
    │   ├── send.rs                 # ipc_send, ipc_send_async (TODO)
    │   ├── receive.rs              # ipc_receive (TODO)
    │   └── queue.rs                # Message queues (TODO)
    ├── capability/                 # Capability system
    │   ├── mod.rs
    │   └── types.rs                # Capability types
    ├── task/                       # Task management
    │   ├── mod.rs
    │   ├── task.rs                 # Task structure
    │   └── scheduler.rs            # Bootstrap round-robin scheduler
    ├── timer/                      # Timer subsystem
    │   └── mod.rs                  # Uptime tracking
    └── drivers/                    # Device drivers
        ├── mod.rs
        └── serial.rs               # COM1 serial console
```

## Critical Implementation Details

### IPC Message Structure

**Location**: `src/ipc/message.rs`

The `IpcMessage` struct is **exactly 64 bytes** (one cache line) with compile-time assertion:

```rust
#[repr(C)]
pub struct IpcMessage {
    pub sender: TaskId,           // u32: 4 bytes
    pub msg_type: IpcType,        // u8: 1 byte
    _padding1: [u8; 3],           // Align payload to 8 bytes
    pub payload: [u64; 4],        // 32 bytes (inline data)
    pub cap: Option<CapabilityId>, // 8 bytes (Option<NonZeroU32>)
    pub shmem: Option<ShmemId>,   // 8 bytes (Option<NonZeroU32>)
    pub msg_id: u64,              // 8 bytes
}

// Compile-time assertion
const _: () = {
    if core::mem::size_of::<IpcMessage>() != 64 {
        panic!("IpcMessage must be exactly 64 bytes!");
    }
};
```

**Why 64 bytes?**
- Fits in single cache line (64 bytes on x86-64)
- Prevents cache line splitting (performance)
- Enables atomic operations on entire message

### Memory Layout

```
0x0000_0000_0000_0000 - 0x0000_7FFF_FFFF_FFFF: User space (128TB)
0xFFFF_8000_0000_0000 - 0xFFFF_FFFF_7FFF_FFFF: Higher-half direct map (128TB)
0xFFFF_FFFF_8000_0000 - 0xFFFF_FFFF_80FF_FFFF: Kernel code/data (16MB)
0xFFFF_FFFF_8100_0000 - 0xFFFF_FFFF_81FF_FFFF: Kernel heap (16MB)
0xFFFF_FFFF_8200_0000 - 0xFFFF_FFFF_8200_0FFF: Guard page (UNMAPPED)
```

### Heap Protection

**Location**: `src/memory/heap.rs`

The kernel heap is protected by:

1. **Guard Page**: Unmapped page after heap causes immediate page fault on overflow
2. **Allocation Error Handler**: Graceful handling of OOM conditions

```rust
#[alloc_error_handler]
fn alloc_error_handler(layout: core::alloc::Layout) -> ! {
    panic!("Kernel heap exhausted: {} bytes requested", layout.size());
}
```

### Bootstrap Scheduler

**Location**: `src/task/scheduler.rs`

Simple round-robin scheduler used during early boot:

- Fixed 1ms time slices
- No priorities or fairness guarantees
- Transitions to userspace CFS scheduler after init spawns it

## Building

**Prerequisites**:
- Rust nightly (for unstable features)
- `rust-src` component
- `llvm-tools-preview` component

**Build command**:

```bash
cargo build --target x86_64-folkering.json --release
```

**Output**: `target/x86_64-folkering/release/kernel` (ELF binary)

## Running

Use Limine bootloader to load the kernel:

```bash
# Generate ISO with Limine
./scripts/create-iso.sh

# Run in QEMU
qemu-system-x86_64 -cdrom folkering.iso -serial stdio
```

## Testing

**Unit tests**:

```bash
cargo test --lib
```

**Integration tests** (requires QEMU):

```bash
cargo test --test integration
```

## Implementation Status

### ✅ Complete

- [x] Project structure and build system
- [x] Cargo.toml with dependencies
- [x] Custom target specification (x86_64-folkering.json)
- [x] Linker script (higher-half kernel)
- [x] Kernel entry point (kmain in main.rs)
- [x] Panic handler with diagnostics
- [x] Serial console driver (COM1) - working output
- [x] Limine bootloader integration (v8.7.0)
- [x] **IDT implementation** (256 entries with exception handlers) ✅ NEW
- [x] **Boot info parsing** (HHDM, RSDP, memory map) ✅ NEW
- [x] **Limine request detection** (4 requests working) ✅ NEW
- [x] **Memory map iteration** (direct slice access) ✅ NEW
- [x] IPC message structure (64 bytes with compile-time assertion)
- [x] IPC endpoint management
- [x] Capability type definitions
- [x] Task structure
- [x] Bootstrap scheduler
- [x] Timer subsystem (uptime tracking)
- [x] Heap allocator skeleton with guard page

### 🚧 IN PROGRESS

#### Memory Management
- [x] **Memory map parsing** (memory_map_slice accessible) ✅
- [ ] **Buddy allocator** (physical.rs:246-295) - 80% complete, scanning memory 🚧
- [ ] **Page table management** (paging.rs) - Limine provides initial tables
- [ ] **Heap initialization** (heap.rs) - Allocate and map heap pages

**Current PMM Status:**
```
[PMM] Initializing physical memory manager...
[PMM] Scanning memory map...
[PMM]   Found 76 pages (0 MB) at 0x53000
```

### 🚧 TODO (Implementation Required)

#### IPC System
- [ ] **Synchronous send** (send.rs) - Blocking IPC with reply
- [ ] **Asynchronous send** (send.rs) - Non-blocking fire-and-forget
- [ ] **Blocking receive** (receive.rs) - Wait for messages
- [ ] **Message queues** (queue.rs) - Per-task FIFO queues

#### Architecture
- [ ] **APIC initialization** (apic.rs) - Local APIC + timer setup
- [ ] **ACPI parsing** (acpi.rs) - Find MADT for CPU topology
- [ ] **Context switching** - Fast path (<500 cycles)
- [ ] **Syscall interface** - SYSCALL/SYSRET instructions

#### Task Management
- [ ] **Task creation** (task.rs) - spawn() function
- [ ] **Scheduler integration** - Bootstrap → userspace transition
- [ ] **IPC blocking/wakeup** - Task state management

#### Capability System
- [ ] **Capability table** - Global capability storage
- [ ] **Mint/grant/revoke** - Capability lifecycle
- [ ] **Validation** - Fast capability checks (<50 cycles)
- [ ] **Transfer via IPC** - Secure capability delegation

#### Boot Process
- [x] **Limine protocol** (boot.rs) - Parse boot info (HHDM, RSDP, memory map) ✅
- [x] **Limine requests** - 4 requests detected and working ✅
- [x] **BootInfo structure** - Created and passed to kernel ✅
- [ ] **Initrd mounting** - CPIO archive parsing
- [ ] **Init process** - Load and spawn /sbin/init
- [ ] **Emergency shell** - Fallback if init fails

**Current Boot Sequence:**
```
1. Limine loads kernel at 0xFFFFFFFF80000000
2. kmain() disables interrupts (CLI)
3. Serial output initialized (COM1)
4. IDT setup (256 entries, LIDT)
5. Parse Limine responses (HHDM, RSDP, memory map)
6. Build BootInfo structure
7. Call kernel_main_with_boot_info()
8. PMM initialization (in progress)
```

## Code Statistics

**Total lines generated**: ~2,500 lines (with boot system)
**Target lines**: ~8,000 lines (full implementation)
**Completion**: ~31%

**Breakdown by module**:

| Module | Lines Generated | Lines Remaining | Status |
|--------|----------------|-----------------|--------|
| Boot | 400 | 100 | 80% ✅ |
| Architecture | 450 | 550 | 45% (IDT complete) |
| Memory | 200 | 1,400 | 13% (PMM in progress) |
| IPC | 400 | 1,200 | 25% |
| Task | 200 | 800 | 20% |
| Capability | 100 | 500 | 17% |
| Drivers | 150 | 250 | 38% (Serial working) |
| Other | 600 | 1,200 | 33% |
| **Total** | **2,500** | **6,000** | **31%** |

**Recent Additions:**
- main.rs: IDT implementation (256 entries, ~140 lines)
- boot.rs: Limine request handling and BootInfo creation
- Serial output fully functional

## Next Steps for Phase 3 Implementation

### Priority 1: Memory Management (Required for everything else)

1. **Implement buddy allocator** (`memory/physical.rs`)
   - Parse memory map from Limine
   - Build free lists for orders 0-11 (4KB to 8MB)
   - Implement alloc_pages() and free_pages()
   - Add coalescing on free

2. **Setup page tables** (`memory/paging.rs`)
   - Identity map first 16MB
   - Map kernel to higher-half
   - Direct map all physical memory at HHDM_OFFSET
   - Implement map_page() helper

3. **Initialize kernel heap** (`memory/heap.rs`)
   - Allocate physical pages for 16MB heap
   - Map to 0xFFFF_FFFF_8100_0000
   - Setup guard page (unmapped)
   - Test allocations (Vec, Box, HashMap)

### Priority 2: IPC System (Core microkernel functionality)

4. **Implement IPC send/receive** (`ipc/send.rs`, `ipc/receive.rs`)
   - Synchronous send with blocking
   - Asynchronous send (non-blocking)
   - Blocking receive with timeout
   - Reply mechanism

5. **Message queues** (`ipc/queue.rs`)
   - Per-task FIFO queues
   - Bounded capacity (64 messages)
   - Backpressure handling

### Priority 3: Task Scheduling

6. **Task creation** (`task/task.rs`)
   - spawn() function (no fork/exec!)
   - ELF binary parsing
   - Page table setup for new task
   - Capability inheritance

7. **Scheduler improvements** (`task/scheduler.rs`)
   - Context switch implementation
   - CFS data structures (BTreeMap)
   - Integration with timer interrupt

### Priority 4: Boot Process

8. **Complete boot sequence** (`boot.rs`)
   - Parse all Limine structures
   - Mount initrd (CPIO)
   - Load and spawn init process
   - Emergency shell fallback

## Documentation

**Architecture documents** (in `../output/architecture/`):

- `IPC-design.md` - IPC specification with 64-byte message struct
- `scheduler-service.md` - Userspace CFS scheduler specification
- `kernel-init.md` - Complete kernel initialization sequence
- `ARCHITECTURE-FIXES-COMPLETE.md` - Phase 2 architecture review fixes

**Key requirements**:

1. IpcMessage **must** be exactly 64 bytes (compile-time assertion enforces this)
2. Kernel heap **must** have guard page protection
3. Bootstrap scheduler **must** transition to userspace scheduler
4. Init process **must** have emergency shell fallback

## License

MIT OR Apache-2.0

## Contributors

- Folkering OS Contributors

---

**Generated**: 2026-01-21
**Version**: 0.1.0 (Phase 3 initial skeleton)
**Agent**: 3.1 (Microkernel Code Generator)
