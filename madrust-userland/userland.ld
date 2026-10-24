/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * MadRust user program linker script.
 *
 * The image is linked at 0x400000, which is where the entry point must be: the
 * kernel logs it and the acceptance of the milestone pins it. The file and
 * program headers therefore stay outside every loadable segment, and the kernel
 * copies the program header table into a user page before it starts the
 * program, so AT_PHDR points at a readable copy.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : ALIGN(16)
    {
        *(.text .text.*)
    }

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(16)
    {
        *(.data .data.*)
        *(.got .got.*)
    }

    .bss : ALIGN(16)
    {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ :
    {
        *(.eh_frame .eh_frame_hdr)
        *(.note.GNU-stack)
        *(.note.gnu.property)
        *(.note.ABI-tag)
        *(.comment)
        *(.llvm_addrsig)
    }

    ASSERT(_start == 0x400000, "the user entry point must be at 0x400000")
}
