/* Layout of every seal_os user program.
 *
 * Linked at USER_ELF_BASE (kernel/src/user.rs): PML4[1], 4 MiB above
 * USER_CODE_START, so it clears both the raw-asm test page and the fixed user
 * stack at USER_CODE_START + 2..6 pages. Never PML4[0], the bootloader's
 * identity map.
 *
 * One PT_LOAD per permission set, each starting on its own page, because a
 * page has exactly one set of permissions: sharing one between code and data
 * would force it writable *and* executable. The kernel loader rejects W+X.
 * The ELF and program headers are not loaded (no FILEHDR/PHDRS in PHDRS), so
 * nothing in the image is readable that the program didn't ask for.
 */
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);   /* R-X */
    rodata PT_LOAD FLAGS(4);   /* R-- */
    data   PT_LOAD FLAGS(6);   /* RW- */
}

SECTIONS
{
    . = 0x8000400000;

    .text : ALIGN(4K) { *(.text .text.*) } :text

    . = ALIGN(4K);
    .rodata : ALIGN(4K) { *(.rodata .rodata.*) } :rodata
    /* Static, non-PIE: GOT slots (if PIC codegen asks for any) are filled in
     * at link time and never written, so they're read-only data. */
    .got : { *(.got .got.*) } :rodata

    . = ALIGN(4K);
    .data : ALIGN(4K) { *(.data .data.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    /* panic = "abort": nothing unwinds, so no unwind tables to load. */
    /DISCARD/ : { *(.eh_frame*) *(.note*) *(.comment) }
}
