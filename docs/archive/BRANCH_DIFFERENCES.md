# AI-Native OS Branch - Key Differences from Master

This document explains how the `ai-native-os` branch differs from the original `master` branch of Folkering OS.

## Branch Purpose

**Master Branch**: General-purpose capability-based microkernel OS
**AI-Native Branch**: Specialized kernel optimized for hosting AI Agents and LLMs

## Philosophical Differences

### Master Branch (General OS)
- Designed for human users with GUI interaction
- Focus on security through capability-based access control
- Traditional process model with files, pipes, and sockets
- Multi-user environment with fair-share scheduling

### AI-Native Branch (This Branch)
- Designed for AI Agents as primary "users"
- Focus on throughput, latency, and determinism
- Agent-based process model with tensor data flow
- Single-tenant by default, deadline-driven scheduling

## Technical Differences

| Component | Master Branch | AI-Native Branch |
|-----------|--------------|------------------|
| **IPC** | Message passing for security | High-bandwidth queues for tensor transfer |
| **Memory** | Standard CoW, page-based | Zero-copy shared memory regions |
| **Scheduler** | Round-robin fair-share | Priority + deadline for inference |
| **Hardware** | Abstracted via capability grants | Direct MMIO access to GPU/NPU |
| **Process Model** | Isolated processes | Agents with shared tensor pools |
| **GUI** | Future window manager | Headless by design, serial debug only |

## Reused Components

The following components from master are reused with minimal changes:

✅ **Physical Memory Manager** - Buddy allocator works for both
✅ **Paging System** - 4-level paging is foundation for both
✅ **GDT/TSS Setup** - Ring 0/3 separation needed for both
✅ **SYSCALL/SYSRET** - Fast syscalls beneficial for both
✅ **Limine Bootloader Integration** - Same boot process

## New/Modified Components

### Added for AI-Native Branch

1. **Shared Memory Objects** (new)
   - Zero-copy tensor sharing between Agents
   - Page-table based access control

2. **Tensor-Aware MessageQueue** (modified)
   - Large message support (GB-sized tensors)
   - Lock-free ring buffer for low latency

3. **Deadline Scheduler** (planned)
   - Priority levels for critical inference
   - Preemption control for GPU-bound tasks

4. **PCI/PCIe Driver Framework** (planned)
   - GPU/NPU enumeration and initialization
   - Direct hardware access for Agents

5. **MMIO Safety Layer** (planned)
   - Safe abstractions for hardware register access
   - Unikernel-style direct access where safe

## Migration Path

If features from AI-Native branch prove useful for general-purpose OS:

1. **Shared Memory Objects** → Could be upstreamed as "High-Performance IPC"
2. **Deadline Scheduler** → Could be optional scheduling policy
3. **MMIO Framework** → Useful for any hardware-accelerated workloads

## Development Strategy

- **Master branch**: Long-term stability, compatibility, security focus
- **AI-Native branch**: Experimental optimizations, AI-first design

Both branches share core kernel infrastructure but diverge in scheduling, IPC, and hardware access strategies.

## When to Use Each Branch

**Use Master Branch if:**
- Building a general-purpose workstation/server OS
- Need multi-user support
- Want traditional Unix-like interface
- Security and isolation are top priority

**Use AI-Native Branch if:**
- Running LLM inference workloads
- Need maximum tensor throughput
- Have GPU/NPU accelerators to utilize
- Latency and determinism are critical
- Agents are the primary "users"

---

**Note**: Both branches maintain compatibility with Rust `no_std` and x86-64 architecture.
