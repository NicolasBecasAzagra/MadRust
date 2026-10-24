/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * 32-bit PVH entry trampoline for MadRust.
 *
 * QEMU enters _start32 in 32-bit protected mode with paging disabled, EBX
 * holding the physical address of hvm_start_info and no usable stack. This
 * code builds a temporary GDT and an identity map of the first 1 GiB with
 * 2 MiB pages, switches to long mode and calls the 64-bit Rust entry.
 */

    .section .text.entry, "ax", @progbits
    .code32
    .global _start32
    .type _start32, @function
_start32:
    cli
    cld

    /* Save the hvm_start_info physical address and install our own stack. */
    movl %ebx, boot_start_info
    movl $__stack_top, %esp

    /* Zero the PML4, the PDPT and the page directory. */
    movl $boot_pml4, %edi
    xorl %eax, %eax
    movl $(3 * 1024), %ecx
    rep stosl

    /* PML4[0] -> PDPT, present + writable. */
    movl $boot_pdpt, %eax
    orl $0x3, %eax
    movl %eax, boot_pml4
    movl $0, boot_pml4 + 4

    /* PDPT[0] -> PD, present + writable. */
    movl $boot_pd, %eax
    orl $0x3, %eax
    movl %eax, boot_pdpt
    movl $0, boot_pdpt + 4

    /* PD: 512 huge pages covering 0x0 .. 0x40000000, present + writable + huge. */
    movl $boot_pd, %edi
    movl $0x83, %eax
    xorl %ecx, %ecx
1:
    movl %eax, (%edi)
    movl $0, 4(%edi)
    addl $0x200000, %eax
    addl $8, %edi
    incl %ecx
    cmpl $512, %ecx
    jb 1b

    /* Enable CR4.PAE. */
    movl %cr4, %eax
    orl $0x20, %eax
    movl %eax, %cr4

    /* Set EFER.LME (MSR 0xC0000080, bit 8). */
    movl $0xC0000080, %ecx
    rdmsr
    orl $(1 << 8), %eax
    wrmsr

    /* Load CR3, then enable paging and protection. */
    movl $boot_pml4, %eax
    movl %eax, %cr3
    movl %cr0, %eax
    orl $0x80000001, %eax
    movl %eax, %cr0

    lgdt boot_gdtr
    ljmp $0x08, $long_mode_start

    .code64
    .global long_mode_start
    .type long_mode_start, @function
long_mode_start:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs
    xorl %ebp, %ebp

    /* RDI = hvm_start_info physical address; VMA == LMA in M0. */
    movl boot_start_info, %edi
    call _start

    cli
2:
    hlt
    jmp 2b

    .section .rodata.boot, "a", @progbits
    .balign 8
boot_gdt:
    .quad 0x0000000000000000
    .quad 0x00AF9A000000FFFF   /* 64-bit code, ring 0, base 0, limit 4 GiB */
    .quad 0x00AF92000000FFFF   /* 64-bit data, ring 0, base 0, limit 4 GiB */
boot_gdt_end:
boot_gdtr:
    .word boot_gdt_end - boot_gdt - 1
    .long boot_gdt

    .section .bss, "aw", @nobits
    .balign 4
boot_start_info:
    .skip 4

    .balign 16
__stack_bottom:
    .skip 16384
    .global __stack_bottom
    .global __stack_top
__stack_top:

    .balign 4096
    .global boot_pml4
boot_pml4:
    .skip 4096
boot_pdpt:
    .skip 4096
boot_pd:
    .skip 4096

/*
 * Xen PVH boot note: XEN_ELFNOTE_PHYS32_ENTRY (0x12), descriptor is the
 * 32-bit physical address of _start32. QEMU reads a 64-bit word here when
 * booting an ELF64 image, so the descriptor is 4 bytes followed by explicit
 * zero padding.
 */
    .section .note.Xen, "a", @note
    .balign 4
    .long 4                   /* n_namesz */
    .long 8                   /* n_descsz: QEMU reads a 64-bit word on ELF64 */
    .long 0x12                /* n_type */
    .asciz "Xen"
    .balign 4
    .long _start32
    .long 0
