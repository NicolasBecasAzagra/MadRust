/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Application processor startup trampoline (milestone E3b).
 *
 * The boot processor copies this blob to physical 0x8000 and patches the five
 * data words at the end. The processor starts in real mode at CS = 0x0800 and
 * IP = 0, so the first byte of the blob is the first instruction. The blob
 * builds a small protected mode GDT, enables PAE and long mode, loads the
 * kernel page tables, and jumps to the higher half entry point with its own
 * stack and its own per processor block.
 *
 * The code is position dependent on purpose: every address below is
 * `AP_TRAMPOLINE_PHYS + offset in this blob`, because the processor executes
 * the copy at 0x8000, not the linked image.
 */

    .set AP_TRAMPOLINE_PHYS, 0x8000
    .set AP_REAL_STACK, 0x7f00

    .section .rodata.ap_trampoline, "a", @progbits
    .balign 16
    .code16
    .global ap_trampoline_start
ap_trampoline_start:
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $AP_REAL_STACK, %sp

    /* The GDT descriptor lives in this blob at its physical address. */
    lgdtl AP_TRAMPOLINE_PHYS + (ap_gdtr - ap_trampoline_start)

    movl %cr0, %eax
    orl $0x1, %eax
    movl %eax, %cr0
    ljmpl $0x08, $AP_TRAMPOLINE_PHYS + (ap_pm32 - ap_trampoline_start)

    .code32
ap_pm32:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* The patched word is the physical address of the kernel PML4. */
    movl AP_TRAMPOLINE_PHYS + (ap_trampoline_cr3 - ap_trampoline_start), %eax
    movl %eax, %cr3

    /* PAE, OSFXSR and OSXMMEXCPT; clear EM and TS in CR0. */
    movl %cr4, %eax
    orl $((1 << 5) | (1 << 9) | (1 << 10)), %eax
    movl %eax, %cr4
    movl %cr0, %eax
    andl $~((1 << 2) | (1 << 3)), %eax
    orl $((1 << 1) | (1 << 5)), %eax
    movl %eax, %cr0
    fninit

    /* EFER.LME. */
    movl $0xc0000080, %ecx
    rdmsr
    orl $(1 << 8), %eax
    wrmsr

    /* Paging on: the CPU is now in compatibility mode. */
    movl %cr0, %eax
    orl $0x80000001, %eax
    movl %eax, %cr0
    ljmpl $0x18, $AP_TRAMPOLINE_PHYS + (ap_lm64 - ap_trampoline_start)

    .code64
ap_lm64:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs

    /* Kernel stack, patched to the higher half by the boot processor. */
    movabsq $(AP_TRAMPOLINE_PHYS + (ap_trampoline_stack - ap_trampoline_start)), %rbx
    movq (%rbx), %rsp

    /* Both GS bases name the per processor block of this processor. */
    movabsq $(AP_TRAMPOLINE_PHYS + (ap_trampoline_percpu - ap_trampoline_start)), %rbx
    movq (%rbx), %rax
    movq %rax, %rdx
    shrq $32, %rdx
    movl $0xc0000101, %ecx
    wrmsr
    movq (%rbx), %rax
    movq %rax, %rdx
    shrq $32, %rdx
    movl $0xc0000102, %ecx
    wrmsr

    /* Processor index is the first argument of the Rust entry point. */
    movabsq $(AP_TRAMPOLINE_PHYS + (ap_trampoline_index - ap_trampoline_start)), %rbx
    movq (%rbx), %rdi
    movabsq $(AP_TRAMPOLINE_PHYS + (ap_trampoline_entry - ap_trampoline_start)), %rbx
    movq (%rbx), %rax
    call *%rax

1:
    cli
    hlt
    jmp 1b

    /* Values the boot processor patches before it sends the start up IPI. */
    .balign 8
    .global ap_trampoline_cr3
ap_trampoline_cr3:
    .quad 0
    .global ap_trampoline_stack
ap_trampoline_stack:
    .quad 0
    .global ap_trampoline_percpu
ap_trampoline_percpu:
    .quad 0
    .global ap_trampoline_index
ap_trampoline_index:
    .quad 0
    .global ap_trampoline_entry
ap_trampoline_entry:
    .quad 0

    .balign 8
ap_gdt:
    .quad 0x0000000000000000
    .quad 0x00cf9a000000ffff   /* 32-bit code, base 0, limit 4 GiB */
    .quad 0x00cf92000000ffff   /* 32-bit data, base 0, limit 4 GiB */
    .quad 0x00af9a000000ffff   /* 64-bit code, base 0, limit 4 GiB */
ap_gdt_end:
ap_gdtr:
    .word ap_gdt_end - ap_gdt - 1
    .long AP_TRAMPOLINE_PHYS + (ap_gdt - ap_trampoline_start)

    .balign 16
    .global ap_trampoline_end
ap_trampoline_end:
