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

    /* Read-only data section.
     *
     * `.lrodata` is the "long rodata" section rustc emits under
     * code-model=medium for >2 GiB-reachable read-only data.
     * Catch it here so it lands in the R-only PHDR alongside
     * regular `.rodata`. Without this, lld leaves `.lrodata`
     * orphaned and merges it into a different segment.
     */
    .rodata ALIGN(4096) :
    {
        *(.rodata .rodata.*)
        *(.lrodata .lrodata.*)
    } :rodata

    /* Executable code section - page aligned */
    . = ALIGN(4096);
    .text :
    {
        *(.text .text.*)
    } :text

    /* Read-write data section - page aligned.
     *
     * `.ldata` and `.lbss` are the "long" data/bss sections rustc
     * emits under code-model=medium for symbols that might exceed
     * the 2 GiB PC-relative reach. Without explicitly gathering
     * them here, lld leaves them orphaned and silently merges them
     * into the preceding R+X PT_LOAD — which causes a #PF on the
     * first write because the kernel applies R+X (no W) page flags
     * based on the segment's ELF flags. This bit synapse hard:
     * its 4.2 MB `SQLITE_BUFFER` was a `.lbss` symbol and the task
     * faulted on first write at 0x214000.
     *
     * The `QUAD(0)` anchor ensures `.data` is non-empty so the
     * writable PHDR stays alive across both small and medium code
     * models even when a binary has no other `.data` symbols.
     */
    . = ALIGN(4096);
    .data :
    {
        QUAD(0);
        *(.data .data.*)
        *(.ldata .ldata.*)
    } :data

    /* BSS (uninitialized data) - same segment as data */
    .bss :
    {
        *(.bss .bss.*)
        *(.lbss .lbss.*)
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
