/* Folkering OS Kernel Linker Script
 * Higher-half kernel at 0xFFFFFFFF80000000
 * Based on limine-rust-template
 */

OUTPUT_FORMAT(elf64-x86-64)

ENTRY(kmain)

/* Program headers for proper MMU permissions */
PHDRS {
    text PT_LOAD;
    rodata PT_LOAD;
    data PT_LOAD;
}

SECTIONS {
    /* Higher-half kernel base - required by Limine spec */
    . = 0xFFFFFFFF80000000;

    __kernel_start = .;

    /* Text section - executable code */
    .text : {
        /* Anchor prevents lld from skipping empty .text section */
        KEEP(*(.text.anchor))
        KEEP(*(.text.boot))
        *(.text .text.*)
        *(.ltext .ltext.*)
    } :text

    /* Move to next page for .rodata */
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* Read-only data */
    .rodata : {
        *(.rodata .rodata.*)
        *(.lrodata .lrodata.*)
        *(.eh_frame_hdr)
        *(.eh_frame)
    } :rodata

    /* Move to next page for .data */
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* Read-write data - Limine requests are embedded here */
    .data : {
        *(.data .data.*)
        *(.ldata .ldata.*)

        /* Limine requests as part of .data section */
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :data

    /* Uninitialized data - MUST be last in :data segment */
    .bss : {
        PROVIDE(__bss_start = .);
        *(.bss .bss.*)
        *(COMMON)
        PROVIDE(__bss_end = .);
    } :data

    __kernel_end = .;

    /* Discard unwanted sections */
    /DISCARD/ : {
        *(.note.GNU-stack)
        *(.note .note.*)
    }
}
