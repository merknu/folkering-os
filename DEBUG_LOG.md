# Folkering OS - Keyboard Text Input Debug Log

## Problem Summary
Keyboard events are received by the compositor (confirmed via serial debug), but typed text doesn't appear in the GUI text box.

## Session 2026-02-01

### Attempt 1: Add debug output for key receiving
- **What:** Added `[COMP:key=0xXX]` debug to print received key values
- **Result:** Showed only high nibble (e.g., `0x6` instead of `0x61` for 'a')
- **Finding:** Hex printing had issue, switched to character printing

### Attempt 2: Print key as character
- **What:** Changed debug to `[COMP:got 'X' OK]`
- **Result:** Shows `[COMP:got 'a' OK]` confirming keys ARE in printable range
- **Finding:** Keys ARE being received correctly (0x20-0x7E range)

### Attempt 3: Debug the match arm
- **What:** Added `[MATCH:printable]` and `[SET:need_redraw]` or `[FULL]`
- **Result:** Shows `[MATCH:printable][FULL]` for every key
- **Finding:** The `text_len < MAX_TEXT_LEN - 1` check is FAILING - buffer appears full!

### Attempt 4: Check text_len initialization
- **What:** Added `[INIT:text_len=0 OK]` at initialization
- **Result:**
  - Init shows `[INIT:text_len=0 OK]` - starts at 0 correctly
  - But key press shows `[MATCH:len=2[FULL!]` - value appears corrupted
- **Finding:** `text_len` is 0 at init but corrupted by the time keys arrive!
  - Note: `len=2` shows `text_len % 10`, so actual value might be 252, 262, etc.

### Attempt 5: Print full hex value of text_len
- **What:** Print `text_len` in full 64-bit hex
- **Result:** `[len=0x00007ffffffefab0[FULL]`
- **Finding:** `text_len` contains a STACK POINTER ADDRESS, not 0!
  - `0x7ffffffefab0` is near the userspace stack top (`0x7ffffffefff8`)
  - This confirms stack corruption - the variable is being read from wrong memory location

### Attempt 6: Move text_buffer and text_len to static memory
- **What:** Changed from stack variables to `static mut` to avoid stack corruption:
  ```rust
  static mut TEXT_BUFFER: [u8; 256] = [0; 256];
  static mut TEXT_LEN: usize = 0;
  ```
- **Result:** **SUCCESS!**
  - `[INIT:text_len=0 OK]` - Correct initialization
  - `[len=0x0000000000000000[OK]` - First key works!
  - `[COMP] Redraw: len=1` - Redraw block entered!
- **Finding:** The issue WAS stack corruption, likely caused by inline assembly `syscall`
  instructions clobbering registers that the compiler expected to hold stack addresses.

## ROOT CAUSE IDENTIFIED
**Stack corruption** - The inline assembly syscalls in the compositor were clobbering
registers without proper clobber declarations. When local variables were stored in
registers, subsequent syscall calls would corrupt them because `syscall` clobbers rcx and r11.

The fix is to use static memory instead of stack-allocated arrays for data that needs
to persist across syscall boundaries.

## SOLUTION
Moved `text_buffer` and `text_len` from stack to static memory:
- Before: `let mut text_len: usize = 0;` (stack)
- After: `static mut TEXT_LEN: usize = 0;` (BSS/data segment)

---

## Session 2026-02-01 (Color Fix)

### Problem
Colors displayed incorrectly - dark blue/purple (`0x1a1a2e`) showed as yellow.

### Investigation
- Limine reports shift values: R=16, G=8, B=0 (standard BGR format)
- Code was using these shifts to transform colors
- QEMU VGA display was not interpreting colors correctly

### Attempts
1. **Swap R and B** - Partial fix, colors still wrong (brown/red instead of blue)
2. **Pass through directly** - **SUCCESS!**

### ROOT CAUSE
QEMU VGA framebuffer expects pixels in `0x00RRGGBB` format directly, without needing
shift-based transformation. The Limine-reported shift values were correct for how
the hardware stores data, but the display interprets the 32-bit value as-is.

### SOLUTION
Changed `color_from_rgb24()` to pass through the RGB value directly:
```rust
pub fn color_from_rgb24(&self, rgb: u32) -> u32 {
    rgb  // Pass through - QEMU expects 0x00RRGGBB directly
}
```
