# AI-Native OS Kernel (Folkering OS AI Branch)

**A specialized microkernel optimized for AI Agents and Large Language Models**, written in Rust for x86-64 architecture.

> **Branch Notice**: This is the `ai-native-os` branch, focused on AI-specific optimizations. For the original general-purpose OS design, see the `master` branch.

## Project Vision

Unlike general-purpose operating systems designed for human-GUI interaction, this OS is optimized for:

- **High-Throughput Data Flow**: Moving large tensors (GBs) between CPU, RAM, and GPU/NPU with zero-copy overhead
- **Low-Latency IPC**: AI Agents communicate via high-performance message buses instantly
- **Deterministic Scheduling**: Critical inference tasks are not preempted unnecessarily

**Read the full architectural context**: [AI_OS_MANIFEST.md](./AI_OS_MANIFEST.md)

## Key Differences from General-Purpose OS

| Feature | Traditional OS | AI-Native OS |
|---------|---------------|--------------|
| **Process Model** | Multi-user, file-based | Agent-based, data-flow oriented |
| **IPC** | Pipes, sockets | High-bandwidth message queues, shared tensors |
| **Scheduling** | Fair-share | Deadline-driven, inference-optimized |
| **Memory** | CoW fork(), swap | Zero-copy regions, no swap |
| **Hardware Access** | Abstracted drivers | Direct MMIO for AI accelerators |

## Current Status

**Phase 1: COMPLETE** ✅
- Physical memory manager (PMM) with buddy allocator
- Page table management (4-level paging)
- Kernel heap allocator (16 MB)
- Higher-half kernel mapping

**Phase 2: COMPLETE** ✅
- GDT/TSS for privilege separation
- SYSCALL/SYSRET fast system calls
- User-mode task execution (Ring 3)
- User↔Kernel transitions working

**Next Steps (AI-Optimized):**
- Fix MessageQueue stack overflow (in-place initialization)
- Shared Memory Objects for zero-copy tensor passing
- Priority + deadline scheduler for inference workloads
- PCI/PCIe enumeration for GPU/NPU drivers
- MMIO framework for safe hardware register access

## Repository Structure

```
folkering-os/
├── kernel/              # Microkernel implementation
│   ├── src/            # Rust source code
│   ├── Cargo.toml      # Kernel dependencies
│   └── build.rs        # Build configuration
├── userspace/          # User-space programs (future)
├── libraries/          # Standard libraries (future)
├── drivers/            # Device drivers (future)
├── tools/              # Build scripts and utilities
├── boot/               # Boot images and configurations
├── docs/               # Technical documentation
└── tests/              # Integration tests
```

## Building

### Prerequisites
- Rust nightly toolchain
- QEMU (for testing)
- Limine bootloader

### Build Commands
```bash
cd kernel
cargo build --target x86_64-folkering.json --release
```

### Testing
```bash
# Boot in QEMU
qemu-system-x86_64 -drive file=boot/boot.img,format=raw,if=ide -serial stdio -m 512M
```

## Documentation

- **Architecture**: See `docs/technical-architecture.md`
- **Development Log**: See Obsidian vault at `Meray_vault/Meray/Projects/Folkering-OS/`
- **Roadmap**: See `docs/ROADMAP.md`

## Development Workflow

1. **Design & Research**: Document in Obsidian vault
2. **Implementation**: Write code in `kernel/`
3. **Testing**: Run in QEMU
4. **Documentation**: Update technical docs in `docs/`

## Links to Obsidian

This repository contains the implementation code. High-level design, research notes, and planning documents are maintained in the Obsidian vault:

`~/OneDrive/Dokumenter/Meray_vault/Meray/Projects/Folkering-OS/`

## License

To be determined.

## Author

Knut Melvær

---

**Built with Rust** 🦀 | **Powered by curiosity** 🚀
