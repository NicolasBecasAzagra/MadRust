/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * MadRust M0 linker script.
 *
 * The kernel is linked identity-mapped at physical 0x100000: VMA == LMA for
 * every section, so the PVH note can carry a plain 32-bit physical address and
 * the 32-bit trampoline can use absolute symbols.
 */

ENTRY(_start)

PHDRS
{
    text PT_LOAD FLAGS(5); /* R + X */
    data PT_LOAD FLAGS(6); /* R + W */
    note PT_NOTE FLAGS(4); /* R     */
}

SECTIONS
{
    . = 0x100000;

    .text : ALIGN(16)
    {
        KEEP(*(.text.entry))
        *(.text .text.*)
    } :text

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    } :text

    /*
     * Xen PVH boot note: XEN_ELFNOTE_PHYS32_ENTRY (0x12), emitted as a real
     * SHT_NOTE input section by trampoline.s. The section keeps a 4-byte
     * alignment on purpose: QEMU aligns the note name to the PT_NOTE program
     * header alignment and then reads the entry address from there. A wider
     * alignment would shift the descriptor.
     */
    .note.Xen : ALIGN(4)
    {
        KEEP(*(.note.Xen))
    } :note

    .data : ALIGN(16)
    {
        *(.data .data.*)
        *(.got .got.*)
    } :data

    .bss : ALIGN(16)
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /*
     * Kernel boundaries. In the M2a identity mapped layout the load addresses
     * and the virtual addresses are the same, so the memory manager reserves
     * the physical range. M2b keeps both pairs apart: the higher-half kernel
     * runs at the virtual addresses while the load addresses stay physical.
     */
    __kernel_start__ = ADDR(.text);
    __kernel_end__ = .;
    __kernel_physical_start__ = LOADADDR(.text);
    __kernel_physical_end__ = LOADADDR(.bss) + SIZEOF(.bss);

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.note.GNU-stack)
        *(.note.gnu.property)
        *(.note.ABI-tag)
        *(.comment)
        *(.llvm_addrsig)
    }

    ASSERT(_start32 == 0x100000, "PVH entry point must be at physical 0x100000")
}
