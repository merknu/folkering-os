# Contributing to Folkering OS

Thank you for your interest in contributing to Folkering OS! This is an AI-native operating system written from scratch in Rust, and every contribution matters.

## How to Contribute

1. **Fork** the repository
2. **Create a branch** for your feature or fix (`git checkout -b feature/my-change`)
3. **Make your changes** following the code style below
4. **Test** your changes (build with `cargo build --release` in both `kernel/` and `userspace/`)
5. **Submit a Pull Request** with a clear description

## Contributor License Agreement (CLA)

**All contributors must sign a CLA before pull requests can be merged.**

By signing the CLA, you grant the project maintainer (Knut Ingmar Merødningen) a perpetual, worldwide, non-exclusive, royalty-free license to sublicense your contributions under commercial terms. You retain full copyright ownership of your contributions.

This is necessary because Folkering OS uses **dual licensing** (AGPL-3.0 + Commercial). Without the CLA, we cannot offer commercial licenses to enterprises.

When you open your first PR, a bot will ask you to sign the CLA electronically. It takes 30 seconds.

## Code Style

- **Language**: Rust (nightly toolchain)
- **Environment**: `#![no_std]` everywhere — no libc, no POSIX
- **Unsafe**: Minimize `unsafe` blocks. Document why each one is necessary.
- **Naming**: snake_case for functions/variables, CamelCase for types
- **Comments**: Only where logic isn't self-evident. No boilerplate doc comments on obvious functions.
- **Error handling**: Return `Result` or error codes. Never `unwrap()` in production paths.
- **Allocation**: Be aware of the heap allocator (free-list, 4MB). Avoid large stack allocations (>4KB).

## Architecture

See [ARCHITECTURE.md](ARCHITECTURE.md) for a complete technical overview.

Key directories:
- `kernel/` — x86-64 kernel (custom target, `cargo build --release`)
- `userspace/` — 6 services + libraries (custom target, `cargo build --release`)
- `tools/` — Build tools, serial proxy, MCP debug servers
- `boot/` — Limine bootloader config, disk images

## What We Need Help With

- **VirtIO drivers**: GPU 3D acceleration, sound, 9P filesystem
- **Networking**: TCP/IP stack improvements, HTTPS client
- **WASM runtime**: New host functions, performance optimization
- **AI inference**: AVX2 GEMM, KV-cache optimization, new model support
- **Testing**: Automated testing framework, CI/CD

## Reporting Issues

Use [GitHub Issues](https://github.com/merknu/folkering-os/issues). Include:
- What you expected vs what happened
- Serial log output (if applicable)
- Steps to reproduce

## Trademark

"Folkering OS" is a trademark of Knut Ingmar Merødningen. Forks may not use the Folkering OS name or branding without permission.

## License

Folkering OS is dual-licensed:
- **Open source**: [AGPL-3.0](LICENSE) — free for open-source use
- **Commercial**: Contact ikkjekvifull@gmail.com for proprietary/closed-source licensing
