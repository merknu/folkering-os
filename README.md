# Folkering OS

A capability-based microkernel operating system written in Rust for x86-64 architecture.

## Project Vision

Folkering OS is a long-term research and development project aimed at creating a secure, modular operating system with a microkernel architecture and capability-based security model.

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

**Next Steps:**
- IPC message passing
- Process/task management
- Capability system
- Scheduler

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
