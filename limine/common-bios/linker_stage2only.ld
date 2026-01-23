OUTPUT_FORMAT(elf32-i386)
ENTRY(_start)
PHDRS
{
    text_s2 PT_LOAD FLAGS(0x05);
    rodata_s2 PT_LOAD FLAGS(0x04);
    data_s2 PT_LOAD FLAGS(0x06);
    text_s3 PT_LOAD FLAGS(0x05);
    rodata_s3 PT_LOAD FLAGS(0x04);
    data_s3 PT_LOAD FLAGS(0x06);
}
SECTIONS
{
    . = 0xf000;
    .text.stage2 : {
        *(.entry)
        *(.realmode)
        *.s2.o(.text .text.*)
    } :text_s2
    .rodata.stage2 : {
        *.s2.o(.rodata .rodata.*)
        build_id_s2 = .;
        KEEP(*build-id.s2.o(*))
        stage2_map = .;
        stage3_common = .;
        build_id_s3 = .;
        full_map = .;
        getchar = .;
        menu = .;
        booting_from_editor = .;
        flanterm_write = .;
        term_backend = .;
        term_fallback = .;
        term_notready = .;
        terms = .;
        terms_i = .;
        usec_at_bootloader_entry = .;
        serial_out = .;
        stage3_addr = .;
    } :rodata_s2
    .data.stage2 : {
        s2_data_begin = .;
        *.s2.o(.data .data.*)
        s2_data_end = .;
    } :data_s2
    .no_unwind.stage2 : {
        *.s2.o(.no_unwind)
    } :data_s2
    .note.gnu.build-id : {
        *(.note.gnu.build-id)
        limine_bios_sys_size = . - 0xf000;
    } :data_s3
    .bss : {
        bss_begin = .;
        *(.bss .bss.*)
        *(COMMON)
        bss_end = .;
    } :data_s3
    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
