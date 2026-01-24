# AI-Native OS Kernel - Architectural Manifest

**ACT AS:** Lead Kernel Architect & Systems Engineer

**PROJECT:** "AI-Native OS Kernel" (Rust, x86_64, Limine Bootloader)

---

## Mission Statement

We are building a specialized **Microkernel designed specifically to host AI Agents and Large Language Models (LLMs) natively**. Unlike general-purpose OSs (Linux/Windows) designed for human-GUI interaction, this OS is optimized for:

1. **High-Throughput Data Flow**: Moving large tensors (GBs) between CPU, RAM, and GPU/NPU with zero-copy overhead
2. **Low-Latency IPC**: AI Agents must communicate via a high-performance message bus (MessageQueue) instantly
3. **Deterministic Scheduling**: Critical inference tasks should not be preempted unnecessarily

---

## Technical Stack

- **Language**: Rust (No-std, core + alloc only)
- **Architecture**: x86_64 (Long Mode)
- **Bootloader**: Limine
- **Memory Model**: Paging with specific focus on Shared Memory regions

---

## Core Kernel Features & Skills Required

### 1. Memory Management (The "Brain")

**Skill**: Advanced Paging & Frame Allocation

**Feature**: Zero-Copy Architecture
- Data loaded from disk/network must be mapped directly to User Space
- Accessible by multiple Agents without duplicating memory (Shared Memory Objects)

**Constraint**:
- Kernel Stack is small (currently ~16KB)
- Avoid large stack allocations
- Use Heap (Box, Vec) for large structures

### 2. Process Management (The "Agents")

**Skill**: Context Switching (Ring 0 ↔ Ring 3) using IRETQ

**Feature**: Isolated Agent Execution
- User Mode tasks are "Agents"
- If an Agent crashes, the Kernel must survive

**State**:
✅ Successfully implemented spawn, switch_to, and restore_context (User Mode is working)

### 3. Inter-Process Communication (The "Synapses")

**Skill**: Lock-free queues, Ring Buffers

**Feature**: Asynchronous Message Passing
- Agents send prompts/tensors to other agents

**Current Task**:
🔄 Implementing MessageQueue initialization without stack overflow (In-Place Initialization)

### 4. Hardware Abstraction (The "Body")

**Skill**: Driver development (Serial, eventually PCI/PCIe for GPU)

**Feature**: Direct Hardware Access
- Give AI Agents safe, direct access to accelerator hardware (Unikernel-style philosophy)
- Via memory mapping, bypassing slow syscall overheads where safe

---

## Development Guidelines

### Strict Type Safety
Leverage Rust's type system to prevent race conditions

### Minimal Context Overhead
Keep the "Hot Path" (syscalls and interrupts) extremely short

### Panic Safety
The kernel must never panic in production. All `unwrap()` must be handled or proven safe

---

## AI-Specific Optimizations

### Tensor Pipeline Architecture
```
[Agent A: LLM Inference]
    ↓ (shared memory)
[Agent B: Post-processing]
    ↓ (message queue)
[Agent C: Output Handler]
```

### Key Design Principles

1. **Zero-Copy Data Movement**: Tensors should never be memcpy'd unnecessarily
2. **Async-First IPC**: MessageQueue must support non-blocking send/receive
3. **Hardware Acceleration**: Direct GPU/NPU access paths for inference workloads
4. **Predictable Latency**: Scheduler must be optimized for inference deadline guarantees

---

## Current Architecture Status

| Component | Status | Notes |
|-----------|--------|-------|
| Memory Paging | ✅ Working | 4-level paging, buddy allocator |
| User Mode | ✅ Working | Ring 3 execution, syscalls functional |
| IPC Foundation | 🔄 In Progress | MessageQueue needs stack-safe init |
| Scheduler | 🔄 Basic | Needs priority + deadline scheduling |
| Shared Memory | 📋 Planned | Zero-copy tensor sharing |
| GPU Drivers | 📋 Future | PCI/PCIe enumeration needed |

---

## Next Steps (Priority Order)

1. **Fix MessageQueue stack overflow** - In-place initialization pattern
2. **Implement Shared Memory Objects** - For zero-copy tensor passing
3. **Enhance Scheduler** - Add priority queues and deadline scheduling
4. **PCI/PCIe Enumeration** - Foundation for GPU drivers
5. **MMIO Framework** - Safe hardware register access for AI accelerators

---

## Architectural Differences from General-Purpose OS

| Feature | General OS | AI-Native OS |
|---------|-----------|--------------|
| Process Model | Multi-user, file-based | Agent-based, data-flow oriented |
| IPC | Pipes, sockets | High-bandwidth message queues, shared tensors |
| Scheduling | Fair-share, time-slicing | Deadline-driven, inference-optimized |
| Memory | CoW fork(), swap | Zero-copy regions, no swap (predictability) |
| Hardware Access | Abstracted via drivers | Direct MMIO where safe (unikernel-style) |
| GUI | Window managers, compositors | Headless by default, terminal for debug |

---

## Philosophy

> "An OS where the primary 'users' are not humans, but AI agents. Every design choice optimizes for throughput, latency, and determinism of data flow between agents and hardware accelerators."

---

*This manifest should be read by all AI coding assistants working on this project to maintain architectural consistency.*
