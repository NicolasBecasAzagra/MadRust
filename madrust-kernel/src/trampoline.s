/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * PVH entry trampoline and switch to the higher half.
 *
 * QEMU enters `_start32` in 32-bit protected mode with paging disabled, EBX
 * holding the physical address of `hvm_start_info` and no usable stack. This
 * code builds a temporary GDT and an identity map of the first 1 GiB with
 * 2 MiB pages, switches to long mode, and then calls `switch_to_higher_half`.
 *
 * `switch_to_higher_half` builds the final page tables in `.bss`: the kernel
 * image at its higher-half virtual address and a direct map of the physical
 * memory the PVH memory map reports as RAM. It loads the new PML4, moves the
 * stack to the higher-half copy of the boot stack and jumps to
 * `higher_half_start`, which drops the identity map and calls the Rust entry.
 *
 * Symbol addresses: the image is linked at KERNEL_VIRTUAL_BASE and loaded at
 * KERNEL_PHYSICAL_BASE, so the physical address of a symbol is
 * `symbol - KERNEL_LOAD_OFFSET`. The 32-bit stubs compute that explicitly,
 * because a 32-bit register cannot hold a higher-half address. The 64-bit
 * switch routine instead runs at its physical address while the identity map is
 * active, so a RIP-relative reference there resolves to the physical address of
 * its target; absolute references are only used for symbols that already hold a
 * physical value.
 */

    .set KERNEL_VIRTUAL_BASE, 0xffffffff80000000
    .set KERNEL_PHYSICAL_BASE, 0x100000
    .set KERNEL_LOAD_OFFSET, KERNEL_VIRTUAL_BASE - KERNEL_PHYSICAL_BASE

    /*
     * Base of the direct map. 0xffff888000000000 sits in the middle of the
     * canonical upper half and is 1 GiB aligned, so a single page directory
     * covers the first GiB of physical memory. It cannot collide with the
     * kernel half at 0xffffffff80000000: the kernel uses PML4 entry 511 and
     * the direct map uses PML4 entry 273.
     */
    .set DIRECT_MAP_BASE, 0xffff888000000000

    /* Page directories reserved for the direct map, 1 GiB each. */
    .set DIRECT_MAP_DIRECTORIES, 32
    .set DIRECT_MAP_MAX, DIRECT_MAP_DIRECTORIES * 0x40000000

    /* Pages of final page tables: PML4 + kernel PDPT/PD/PT + direct PDPT + PDs. */
    .set TABLE_PAGES, 5 + DIRECT_MAP_DIRECTORIES

    .section .text.entry, "ax", @progbits
    .code32
    .global _start32
    .type _start32, @function
_start32:
    cli
    cld

    /*
     * Zero the kernel .bss. The ELF format does not require the loader to
     * clear it, and the boot stack, the page tables and the static allocator
     * metadata all live there, so the kernel clears it itself.
     */
    movl $(__bss_start__ - KERNEL_LOAD_OFFSET), %edi
    movl $(__bss_end__ - KERNEL_LOAD_OFFSET), %ecx
    subl %edi, %ecx
    shrl $2, %ecx
    xorl %eax, %eax
    rep stosl

    /* Save the hvm_start_info physical address and install the boot stack. */
    movl %ebx, (boot_start_info - KERNEL_LOAD_OFFSET)
    movl $(__stack_top - KERNEL_LOAD_OFFSET), %esp

    /* Zero the temporary PML4, the PDPT and the page directory. */
    movl $(boot_pml4 - KERNEL_LOAD_OFFSET), %edi
    xorl %eax, %eax
    movl $(3 * 1024), %ecx
    rep stosl

    /* PML4[0] -> PDPT, present + writable. */
    movl $(boot_pdpt - KERNEL_LOAD_OFFSET), %eax
    orl $0x3, %eax
    movl %eax, (boot_pml4 - KERNEL_LOAD_OFFSET)
    movl $0, (boot_pml4 - KERNEL_LOAD_OFFSET + 4)

    /* PDPT[0] -> PD, present + writable. */
    movl $(boot_pd - KERNEL_LOAD_OFFSET), %eax
    orl $0x3, %eax
    movl %eax, (boot_pdpt - KERNEL_LOAD_OFFSET)
    movl $0, (boot_pdpt - KERNEL_LOAD_OFFSET + 4)

    /* PD: 512 huge pages covering 0x0 .. 0x40000000, present + writable + huge. */
    movl $(boot_pd - KERNEL_LOAD_OFFSET), %edi
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

    /*
     * Enable the floating point unit and SSE. A standard library program and
     * the musl string routines it calls use SSE2 instructions such as movaps;
     * the CPU raises #UD on them unless CR4.OSFXSR is set and CR0.EM is clear.
     * Long mode always has SSE2, so the bits are safe on every CPU that gets
     * this far. CR4.PAE is set in the same write.
     */
    movl %cr0, %eax
    andl $0xffffffef, %eax        /* TS = 0: no lazy context switch */
    andl $0xfffffffb, %eax        /* EM = 0: no instruction emulation */
    orl $0x22, %eax               /* MP and NE */
    movl %eax, %cr0

    movl %cr4, %eax
    orl $(0x20 | (1 << 9) | (1 << 10)), %eax  /* PAE, OSFXSR, OSXMMEXCPT */
    movl %eax, %cr4

    fninit

    /* Set EFER.LME (MSR 0xC0000080, bit 8). */
    movl $0xC0000080, %ecx
    rdmsr
    orl $(1 << 8), %eax
    wrmsr

    /* Load CR3, then enable paging and protection. */
    movl $(boot_pml4 - KERNEL_LOAD_OFFSET), %eax
    movl %eax, %cr3
    movl %cr0, %eax
    orl $0x80000001, %eax
    movl %eax, %cr0

    movl $(boot_gdtr - KERNEL_LOAD_OFFSET), %eax
    lgdt (%eax)
    ljmp $0x08, $(long_mode_start - KERNEL_LOAD_OFFSET)

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

    /* RDI = hvm_start_info physical address; the image is identity mapped. */
    movl boot_start_info(%rip), %edi
    jmp switch_to_higher_half

/*
 * Build the final page tables, load them and jump to the higher half.
 *
 * Input:  RDI = physical address of `hvm_start_info`.
 * Output: never returns; it calls `_start` with the start info physical
 *         address, the probed physical address and the value read there
 *         before the switch.
 *
 * Every access below is RIP-relative or a physical constant, because the CPU
 * still runs at the physical address of this routine.
 *
 * Registers kept across the whole routine:
 *   R15 = hvm_start_info physical address
 *   R14 = physical address probed before the switch
 *   R13 = highest RAM end from the PVH memory map, clamped to DIRECT_MAP_MAX
 *   R12 = value read at R14 while the identity map was still active
 *
 * The unclamped highest RAM end is kept in `boot_highest_ram`, so the Rust
 * entry point can refuse a guest the reserved page directories cannot map
 * instead of running with part of its memory unreachable.
 */
    .global switch_to_higher_half
    .type switch_to_higher_half, @function
switch_to_higher_half:
    movq %rdi, %r15

    /*
     * Probe one physical address that the kernel only reaches through the
     * direct map after the switch. The start of the PVH memory map is inside
     * the low RAM QEMU filled in; an absent map falls back to the start info.
     */
    movq 40(%r15), %r14
    testq %r14, %r14
    jnz 2f
    movq %r15, %r14
2:
    movq (%r14), %r12

    /* Highest RAM end, so the direct map covers every usable byte. */
    xorq %r13, %r13
    movq 40(%r15), %rsi
    movl 48(%r15), %r9d
    testq %rsi, %rsi
    jz 5f
    testl %r9d, %r9d
    jz 5f
    xorl %r8d, %r8d
3:
    cmpl %r9d, %r8d
    jae 5f
    cmpl $1, 16(%rsi)
    jne 4f
    movq (%rsi), %rax
    addq 8(%rsi), %rax
    cmpq %r13, %rax
    jbe 4f
    movq %rax, %r13
4:
    addq $24, %rsi
    incl %r8d
    jmp 3b
5:

    /* Zero the final page tables. */
    leaq mm_pml4(%rip), %rdi
    movl $(TABLE_PAGES * 1024), %ecx
    xorl %eax, %eax
    rep stosl

    /*
     * PML4[0] keeps the identity map of the first GiB that the 32-bit stub
     * built, so this code stays fetchable after CR3 is reloaded. It is removed
     * again in `higher_half_start`.
     */
    leaq boot_pdpt(%rip), %rax
    orq $0x3, %rax
    leaq mm_pml4(%rip), %rdx
    movq %rax, (%rdx)

    /* PML4[kernel] -> kernel PDPT. */
    movabsq $KERNEL_VIRTUAL_BASE, %rax
    movq %rax, %rcx
    shrq $39, %rcx
    andq $0x1ff, %rcx
    leaq mm_pdpt_kernel(%rip), %rax
    orq $0x3, %rax
    movq %rax, (%rdx,%rcx,8)

    /* PDPT[kernel] -> kernel PD. */
    movabsq $KERNEL_VIRTUAL_BASE, %rax
    movq %rax, %rcx
    shrq $30, %rcx
    andq $0x1ff, %rcx
    leaq mm_pd_kernel(%rip), %rax
    orq $0x3, %rax
    leaq mm_pdpt_kernel(%rip), %rdx
    movq %rax, (%rdx,%rcx,8)

    /* PD[kernel] -> 4 KiB page table for the first 2 MiB of the image. */
    movabsq $KERNEL_VIRTUAL_BASE, %rax
    movq %rax, %rcx
    shrq $21, %rcx
    andq $0x1ff, %rcx
    leaq mm_pt_kernel(%rip), %rax
    orq $0x3, %rax
    leaq mm_pd_kernel(%rip), %rdx
    movq %rax, (%rdx,%rcx,8)

    /*
     * The image starts at 1 MiB, which is not 2 MiB aligned, so the first
     * 2 MiB of the kernel half is mapped with 4 KiB pages: the page table
     * covers physical 0x100000 to 0x300000.
     */
    leaq mm_pt_kernel(%rip), %rdi
    movl $KERNEL_PHYSICAL_BASE, %r8d
    xorl %ecx, %ecx
6:
    movq %r8, %rax
    orq $0x3, %rax
    movq %rax, (%rdi,%rcx,8)
    addq $4096, %r8
    incl %ecx
    cmpl $512, %ecx
    jb 6b

    /*
     * The rest of the image uses 2 MiB pages, which require a 2 MiB aligned
     * physical address: r8 is at 0x300000 after the loop above. The loop
     * covers the image rounded up to a 2 MiB boundary.
     */
    leaq mm_pd_kernel(%rip), %rdi
    movl $1, %ecx
    movabsq $__kernel_physical_end__, %rdx
    addq $0x1fffff, %rdx
    andq $-0x200000, %rdx
8:
    cmpq %rdx, %r8
    jae 9f
    movq %r8, %rax
    orq $0x83, %rax
    movq %rax, (%rdi,%rcx,8)
    addq $0x200000, %r8
    incl %ecx
    cmpl $512, %ecx
    jb 8b
9:

    /* Keep the unclamped highest RAM end for the Rust refusal check, then
     * limit the mapping to the page directories this file reserves. */
    movq %r13, boot_highest_ram(%rip)
    movabsq $DIRECT_MAP_MAX, %rax
    cmpq %rax, %r13
    jbe 10f
    movq %rax, %r13
10:

    /* PML4[direct] -> direct map PDPT. */
    movabsq $DIRECT_MAP_BASE, %rax
    movq %rax, %rcx
    shrq $39, %rcx
    andq $0x1ff, %rcx
    leaq mm_pdpt_direct(%rip), %rax
    orq $0x3, %rax
    leaq mm_pml4(%rip), %rdx
    movq %rax, (%rdx,%rcx,8)

    /*
     * One page directory per GiB of physical memory, each entry a 2 MiB page
     * of the identity translation physical -> DIRECT_MAP_BASE + physical.
     */
    leaq mm_pdpt_direct(%rip), %rbx
    leaq mm_pd_direct(%rip), %rdi
    xorl %r10d, %r10d
    xorq %r8, %r8
11:
    movq %rdi, %rax
    orq $0x3, %rax
    movq %rax, (%rbx,%r10,8)
    xorl %ecx, %ecx
12:
    cmpq %r13, %r8
    jae 13f
    movq %r8, %rax
    orq $0x83, %rax
    movq %rax, (%rdi,%rcx,8)
    addq $0x200000, %r8
    incl %ecx
    cmpl $512, %ecx
    jb 12b
13:
    cmpq %r13, %r8
    jae 14f
    addq $4096, %rdi
    incl %r10d
    jmp 11b
14:

    /*
     * Load the final PML4 and move to the higher-half stack. Nothing between
     * the CR3 write and the jump may touch the stack or an absolute reference:
     * the stack pointer still holds a physical address and only the identity
     * map keeps this instruction stream fetchable.
     */
    leaq mm_pml4(%rip), %rax
    movq %rax, %cr3
    movabsq $__stack_top, %rsp
    movabsq $higher_half_start, %rax
    jmp *%rax

/*
 * First instruction of the kernel that runs from the higher half.
 *
 * It removes the identity map, so from here on physical memory is only
 * reachable through the direct map, and hands the boot data to Rust.
 */
    .global higher_half_start
    .type higher_half_start, @function
higher_half_start:
    leaq mm_pml4(%rip), %rax
    movq $0, (%rax)
    movq %cr3, %rax
    movq %rax, %cr3

    /* RDI = hvm_start_info, RSI = probe address, RDX = probe value,
     * RCX = highest RAM end from the memory map. */
    movq %r15, %rdi
    movq %r14, %rsi
    movq %r12, %rdx
    movq boot_highest_ram(%rip), %rcx
    call _start

    cli
15:
    hlt
    jmp 15b

    .section .rodata.boot, "a", @progbits
    .balign 8
boot_gdt:
    .quad 0x0000000000000000
    .quad 0x00AF9A000000FFFF   /* 64-bit code, ring 0, base 0, limit 4 GiB */
    .quad 0x00AF92000000FFFF   /* 64-bit data, ring 0, base 0, limit 4 GiB */
boot_gdt_end:
boot_gdtr:
    .word boot_gdt_end - boot_gdt - 1
    .long boot_gdt - KERNEL_LOAD_OFFSET

    .section .bss, "aw", @nobits
    .balign 4
boot_start_info:
    .skip 4

    /* Highest RAM end from the PVH memory map, before the direct map limit. */
    .balign 8
    .global boot_highest_ram
boot_highest_ram:
    .skip 8

    .balign 16
    .skip 16384
    .global __stack_top
__stack_top:

    .balign 4096
boot_pml4:
    .skip 4096
    .global boot_pdpt
boot_pdpt:
    .skip 4096
boot_pd:
    .skip 4096

    /*
     * Final page tables. They live in `.bss`, so the kernel image mapping
     * covers them and the memory manager reserves them with the image.
     */
    .balign 4096
    .global mm_pml4
mm_pml4:
    .skip 4096
mm_pdpt_kernel:
    .skip 4096
mm_pd_kernel:
    .skip 4096
mm_pt_kernel:
    .skip 4096
    .global mm_pdpt_direct
mm_pdpt_direct:
    .skip 4096
    .global mm_pd_direct
mm_pd_direct:
    .skip 4096 * DIRECT_MAP_DIRECTORIES

    /*
     * Keep this block contiguous and in this order: `switch_to_higher_half`
     * zeroes it as one range of TABLE_PAGES pages starting at `mm_pml4`.
     */

/*
 * Xen PVH boot note: XEN_ELFNOTE_PHYS32_ENTRY (0x12), descriptor is the
 * 32-bit physical address of _start32, which is the virtual address minus the
 * load offset. QEMU reads a 64-bit word here when booting an ELF64 image, so
 * the descriptor is 4 bytes followed by explicit zero padding.
 */
    .section .note.Xen, "a", @note
    .balign 4
    .long 4                   /* n_namesz */
    .long 8                   /* n_descsz: QEMU reads a 64-bit word on ELF64 */
    .long 0x12                /* n_type */
    .asciz "Xen"
    .balign 4
    .long _start32 - KERNEL_LOAD_OFFSET
    .long 0
