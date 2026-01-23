# Rebuild Instructions - Updated Approach

## What Changed

The `build.rs` approach failed because Windows doesn't have GNU `as` assembler.

**New approach**: Using `global_asm!` macro in `lib.rs` to inline the boot assembly.

## Changes Made

**File: `src/lib.rs`** - Added lines 28-29:
```rust
use core::arch::global_asm;
global_asm!(include_str!("arch/x86_64/boot.S"));
```

This will compile `boot.S` directly using Rust's inline assembler.

## Rebuild Steps

Open PowerShell or Command Prompt:

```cmd
cd "C:\Users\merkn\OneDrive\Dokumenter\Meray_vault\Meray\Projects\Folkering-OS\code\kernel"

REM Clean previous build
cargo clean

REM Rebuild with the assembly included
cargo build --release
```

## Verify the Fix

After building, check that the kernel has all sections:

```cmd
REM Check sections (should now have .text.boot, .text, .bss)
llvm-readelf -S target\x86_64-folkering\release\kernel

REM Check entry point (should NOT be 0x0)
llvm-readelf -h target\x86_64-folkering\release\kernel | findstr "Entry"
```

## Expected Output

You should see:
- `.text.boot` section containing the `_start` entry point
- `.text` section with kernel code
- `.bss` section for uninitialized data
- Entry point address: `0xffffffffc0000000` (or similar, NOT 0x0)
- Kernel size should be larger than 1.3KB (probably 30-100KB)

## After Successful Build

Copy the kernel back to WSL:

```cmd
copy target\x86_64-folkering\release\kernel \\wsl$\Ubuntu\home\knut\folkering\kernel\target\x86_64-folkering\release\kernel
```

Then in WSL, rebuild the ISO and boot it!

---

**Note**: The `build.rs` file is no longer needed and can be deleted, but leaving it won't hurt anything.
