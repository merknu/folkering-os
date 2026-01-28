/* Folkering OS Userspace Linker Script
 *
 * Ensures sections are page-aligned (4KB) so the kernel can
 * map them with correct permissions:
 *   - .rodata: R (Read-only)
 *   - .text:   R+X (Read+Execute)
 *   - .data:   R+W (Read+Write)
 *   - .bss:    R+W (Read+Write)
 */

ENTRY(_start)

PHDRS
{
    rodata PT_LOAD FLAGS(4);     /* R */
    text   PT_LOAD FLAGS(5);     /* R+X */
    data   PT_LOAD FLAGS(6);     /* R+W */
}

SECTIONS
{
    /* Start at a reasonable userspace address */
    . = 0x200000;

    /* Read-only data section */
    .rodata ALIGN(4096) :
    {
        *(.rodata .rodata.*)
    } :rodata

    /* Executable code section - page aligned */
    . = ALIGN(4096);
    .text :
    {
        *(.text .text.*)
    } :text

    /* Read-write data section - page aligned */
    . = ALIGN(4096);
    .data :
    {
        *(.data .data.*)
    } :data

    /* BSS (uninitialized data) - same segment as data */
    .bss :
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /* Discard unnecessary sections */
    /DISCARD/ :
    {
        *(.eh_frame)
        *(.note*)
        *(.comment)
    }
}
