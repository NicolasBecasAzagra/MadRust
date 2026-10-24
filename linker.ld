/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * MadRust M2b linker script.
 *
 * The kernel is linked at the higher half, 0xffffffff80000000, but loaded at
 * physical 0x100000. Every loadable section therefore carries an explicit load
 * address through AT(), so the ELF program headers describe the physical
 * placement while the symbols carry the virtual addresses the C and Rust code
 * runs at.
 *
 * The 32-bit entry point is the first byte of the image, so its physical
 * address is 0x100000 and its virtual address is the kernel virtual base; the
 * PVH note and the ASSERT below check both.
 */

ENTRY(_start)

KERNEL_VIRTUAL_BASE = 0xffffffff80000000;
KERNEL_PHYSICAL_BASE = 0x100000;

PHDRS
{
    text PT_LOAD FLAGS(5); /* R + X */
    data PT_LOAD FLAGS(6); /* R + W */
    note PT_NOTE FLAGS(4); /* R     */
}

SECTIONS
{
    . = KERNEL_VIRTUAL_BASE;

    .text : AT(ADDR(.text) - KERNEL_VIRTUAL_BASE + KERNEL_PHYSICAL_BASE) ALIGN(16)
    {
        KEEP(*(.text.entry))
        *(.text .text.*)
    } :text

    .rodata : AT(ADDR(.rodata) - KERNEL_VIRTUAL_BASE + KERNEL_PHYSICAL_BASE) ALIGN(16)
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
    .note.Xen : AT(ADDR(.note.Xen) - KERNEL_VIRTUAL_BASE + KERNEL_PHYSICAL_BASE) ALIGN(4)
    {
        KEEP(*(.note.Xen))
    } :note

    .data : AT(ADDR(.data) - KERNEL_VIRTUAL_BASE + KERNEL_PHYSICAL_BASE) ALIGN(16)
    {
        *(.data .data.*)
        *(.got .got.*)
    } :data

    .bss : AT(ADDR(.bss) - KERNEL_VIRTUAL_BASE + KERNEL_PHYSICAL_BASE) ALIGN(16)
    {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /*
     * Kernel boundaries, as two pairs.
     *
     * `__kernel_start__` / `__kernel_end__` are virtual addresses; they are
     * what the kernel prints and what names the higher half image.
     * `__kernel_physical_start__` / `__kernel_physical_end__` are load
     * addresses; the physical end covers `.bss`, so the memory manager
     * reserves the whole image including the boot stack, the boot page tables
     * and the static metadata.
     */
    __kernel_start__ = ADDR(.text);
    __kernel_end__ = ADDR(.bss) + SIZEOF(.bss);
    __kernel_physical_start__ = LOADADDR(.text);
    __kernel_physical_end__ = LOADADDR(.bss) + SIZEOF(.bss);

    /*
     * Bounds of the zero initialised data. The 32-bit entry point clears it
     * before it uses anything, because the ELF format does not require the
     * loader to do it.
     */
    __bss_start__ = ADDR(.bss);
    __bss_end__ = ADDR(.bss) + SIZEOF(.bss);

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.note.GNU-stack)
        *(.note.gnu.property)
        *(.note.ABI-tag)
        *(.comment)
        *(.llvm_addrsig)
    }

    ASSERT(LOADADDR(.text) == KERNEL_PHYSICAL_BASE,
           "the kernel image must be loaded at physical 0x100000")
    ASSERT(_start32 - KERNEL_VIRTUAL_BASE + KERNEL_PHYSICAL_BASE == KERNEL_PHYSICAL_BASE,
           "PVH entry point must be at physical 0x100000")
    ASSERT(_start32 == KERNEL_VIRTUAL_BASE,
           "PVH entry point must be the first byte of the higher half image")
    ASSERT(LOADADDR(.bss) + SIZEOF(.bss) <= KERNEL_PHYSICAL_BASE + 0x200000,
           "the kernel image must fit in the 2 MiB mapped with 4 KiB pages")
}
