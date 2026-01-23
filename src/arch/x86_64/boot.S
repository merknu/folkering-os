/* Folkering OS Boot Assembly
 * Entry point from bootloader to kernel_main()
 */

.section .text.boot
.global _start
.code64

/* Kernel entry point
 * RDI = unused (Limine doesn't pass arguments in registers)
 * Boot info is found via Limine request/response system
 */
_start:
    /* 1. Disable interrupts (should already be disabled by bootloader) */
    cli

    /* 2. Verify we're in long mode (64-bit)
     * Check EFER.LMA (Long Mode Active) bit
     */
    mov $0xC0000080, %ecx       # IA32_EFER MSR
    rdmsr                       # Read MSR into EDX:EAX
    test $(1 << 10), %eax       # Test LMA bit (bit 10)
    jz .not_long_mode

    /* 3. Setup initial stack (16KB for boot CPU) */
    lea boot_stack_top(%rip), %rsp
    mov %rsp, %rbp

    /* 4. Clear direction flag (for string operations) */
    cld

    /* 5. Clear BSS section (uninitialized data) */
    lea __bss_start(%rip), %rdi
    lea __bss_end(%rip), %rcx
    sub %rdi, %rcx
    xor %rax, %rax
    rep stosb

    /* 6. Jump to Rust kernel_main */
    call kernel_main

    /* Should never return, but halt if it does */
.halt:
    cli
    hlt
    jmp .halt

/* Error: Not in long mode */
.not_long_mode:
    /* Write error message to VGA text buffer (legacy fallback) */
    mov $0xB8000, %rdi
    movl $0x4F524F45, 0(%rdi)     # "E" (red on white)
    movl $0x4F524F52, 2(%rdi)     # "R" (red on white)
    movl $0x4F524F52, 4(%rdi)     # "R" (red on white)
    movl $0x4F524F4F, 6(%rdi)     # "O" (red on white)
    movl $0x4F524F52, 8(%rdi)     # "R" (red on white)
    jmp .halt

/* Boot stack (16KB for boot CPU) */
.section .bss
.align 4096
boot_stack_bottom:
    .skip 16384
boot_stack_top:

/* Additional stacks for SMP (16 CPUs max, 16KB each) */
    .skip (16384 * 15)
boot_stacks_end:
