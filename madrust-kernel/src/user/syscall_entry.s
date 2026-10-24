/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Ring 3 entry, system call entry and system call return.
 *
 * madrust_enter_user_mode(entry, user_stack) saves the kernel stack pointer,
 * builds an iretq frame with the ring 3 code and data selectors and returns to
 * ring 0 only when the program leaves through madrust_user_return.
 *
 * madrust_syscall_entry is the LSTAR target. The CPU arrives in ring 0 with
 * the user stack pointer still in RSP, the user return address in RCX, the
 * user flags in R11 and interrupts masked by IA32_SFMASK. The stub:
 *   1. swaps to the kernel GS base and saves the user RSP in the per-CPU slot,
 *   2. loads the kernel stack pointer from TSS.rsp0 (the same stack the CPU
 *      installs for an interrupt from ring 3) through MADRUST_TSS_ADDRESS,
 *   3. saves every general purpose register plus the user RIP and flags,
 *   4. calls the Rust dispatcher with the Linux argument order,
 *   5. restores the registers, reloads the user RSP and returns with sysretq.
 */

    .section .bss.userentry, "aw", @nobits
    .balign 8
/*
 * Kernel registers of the frame that called madrust_enter_user_mode.
 *
 * The user program and the system call stubs overwrite the callee saved
 * registers, and the compiler expects them back unchanged when the call
 * returns. They are saved once here and restored by madrust_user_return.
 */
saved_kernel_rsp:
    .skip 8
saved_kernel_rbp:
    .skip 8
saved_kernel_rbx:
    .skip 8
saved_kernel_r12:
    .skip 8
saved_kernel_r13:
    .skip 8
saved_kernel_r14:
    .skip 8
saved_kernel_r15:
    .skip 8

    .section .text.userentry, "ax", @progbits
    .code64

/*
 * void madrust_enter_user_mode(u64 entry, u64 user_stack) -> u64
 *
 * Returns the status passed to madrust_user_return, which is the exit status
 * of the user program.
 */
    .global madrust_enter_user_mode
    .type madrust_enter_user_mode, @function
madrust_enter_user_mode:
    movq %rsp, saved_kernel_rsp(%rip)
    movq %rbp, saved_kernel_rbp(%rip)
    movq %rbx, saved_kernel_rbx(%rip)
    movq %r12, saved_kernel_r12(%rip)
    movq %r13, saved_kernel_r13(%rip)
    movq %r14, saved_kernel_r14(%rip)
    movq %r15, saved_kernel_r15(%rip)

    /* Interrupts stay masked until iretq restores RFLAGS in ring 3. */
    cli

    movq %rdi, %rax
    movq %rsi, %rdx

    /* Ring 3 data for the segments the iretq target will use. */
    movw $0x23, %cx
    movw %cx, %ds
    movw %cx, %es

    /* iretq frame, pushed from the lowest address to the highest. */
    pushq $0x23          /* SS: ring 3 data */
    pushq %rdx           /* RSP: user stack pointer */
    pushq $0x202         /* RFLAGS: reserved bit 1 and IF */
    pushq $0x1b          /* CS: ring 3 code */
    pushq %rax           /* RIP: program entry point */
    iretq

/*
 * void madrust_user_return(u64 status) -> !
 *
 * Called from the dispatcher when the program exits. It discards the system
 * call frame and returns into madrust_enter_user_mode with the status in RAX.
 */
    .global madrust_user_return
    .type madrust_user_return, @function
madrust_user_return:
    movq %rdi, %rax
    swapgs
    movq saved_kernel_rsp(%rip), %rsp
    movq saved_kernel_rbp(%rip), %rbp
    movq saved_kernel_rbx(%rip), %rbx
    movq saved_kernel_r12(%rip), %r12
    movq saved_kernel_r13(%rip), %r13
    movq saved_kernel_r14(%rip), %r14
    movq saved_kernel_r15(%rip), %r15
    ret

/*
 * System call entry. The Linux convention puts the number in RAX, the
 * arguments in RDI, RSI, RDX, R10, R8 and R9, and the result in RAX.
 */
    .global madrust_syscall_entry
    .type madrust_syscall_entry, @function
madrust_syscall_entry:
    /*
     * The user GS base is not valid kernel memory, so swap first and only then
     * store the user stack pointer in the per-CPU area at GS:0.
     */
    swapgs
    movq %rsp, %gs:0

    /* RSP := TSS.rsp0, the ring 0 stack for a transition from ring 3. */
    movq MADRUST_TSS_ADDRESS(%rip), %rsp
    movq 4(%rsp), %rsp

    cld

    /*
     * Save the user context. RCX and R11 are at the top because sysretq needs
     * them back unchanged; the frame order is documented below.
     */
    pushq %rcx           /* 112: user RIP */
    pushq %r11           /* 104: user RFLAGS */
    pushq %rax           /*  96: syscall number */
    pushq %rbx           /*  88 */
    pushq %rdx           /*  80: third argument */
    pushq %rsi           /*  72: second argument */
    pushq %rdi           /*  64: first argument */
    pushq %rbp           /*  56 */
    pushq %r8            /*  48: fifth argument */
    pushq %r9            /*  40: sixth argument */
    pushq %r10           /*  32: fourth argument */
    pushq %r12           /*  24 */
    pushq %r13           /*  16 */
    pushq %r14           /*   8 */
    pushq %r15           /*   0 */

    /* Publish the frame for `clone`, which copies the parent registers. */
    movq %rsp, %gs:8

    /*
     * Marshal the Linux argument order into the SysV order of
     * madrust_syscall_dispatch(number, a1, a2, a3, a4, a5, a6): the sixth
     * argument goes on the stack, the other six into registers.
     */
    movq 96(%rsp), %rdi  /* number */
    movq 64(%rsp), %rsi  /* a1 */
    movq 72(%rsp), %rdx  /* a2 */
    movq 80(%rsp), %rcx  /* a3 */
    movq 32(%rsp), %r8   /* a4, from the syscall's R10 */
    movq 48(%rsp), %r9   /* a5, from the syscall's R8 */
    movq 40(%rsp), %rax  /* a6, from the syscall's R9 */
    pushq %rax

    call madrust_syscall_dispatch

    /* Drop the stack argument, restore every register but RAX. */
    addq $8, %rsp
    popq %r15
    popq %r14
    popq %r13
    popq %r12
    popq %r10
    popq %r9
    popq %r8
    popq %rbp
    popq %rdi
    popq %rsi
    popq %rdx
    popq %rbx
    addq $8, %rsp        /* saved RAX: the dispatcher result is already in RAX */
    popq %r11            /* user RFLAGS for sysretq */
    popq %rcx            /* user RIP for sysretq */

    movq %gs:0, %rsp
    swapgs
    sysretq

/*
 * void madrust_enter_user_thread(const struct UserStart *start) -> !
 *
 * First entry of a thread created by clone. The pointer is in RDI; R11 becomes
 * the scratch register because the syscall instruction already clobbers it.
 * The saved context is loaded, an iretq frame is built from it, and the CPU
 * leaves ring 0 with the child's registers and stack. R11 and RCX are not
 * restored: the syscall ABI treats them as clobbered.
 */
    .global madrust_enter_user_thread
    .type madrust_enter_user_thread, @function
madrust_enter_user_thread:
    movq %rdi, %r11
    cli

    movw $0x23, %cx
    movw %cx, %ds
    movw %cx, %es

    pushq $0x23          /* SS: ring 3 data */
    pushq 8(%r11)        /* RSP: child stack */
    pushq 16(%r11)       /* RFLAGS: interrupt flag set by the caller */
    pushq $0x1b          /* CS: ring 3 code */
    pushq 0(%r11)        /* RIP: syscall return address */

    movq 24(%r11), %rax
    movq 32(%r11), %rbx
    movq 40(%r11), %rbp
    movq 48(%r11), %r12
    movq 56(%r11), %r13
    movq 64(%r11), %r14
    movq 72(%r11), %r15
    movq 80(%r11), %rdi
    movq 88(%r11), %rsi
    movq 96(%r11), %rdx
    movq 104(%r11), %r8
    movq 112(%r11), %r9
    movq 120(%r11), %r10

    iretq
