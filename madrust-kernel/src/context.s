/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * Context switch and new-thread entry for MadRust (M3).
 *
 * madrust_context_switch saves the callee saved registers of the running
 * thread into the first argument and loads the second. It also stores the
 * stack pointer, but not RIP: the Rust caller writes the resume address into
 * `Context::rip` before the call. That address is `madrust_context_resume`,
 * a single `ret` that pops the return address of the call to this routine,
 * so a resumed thread continues with exactly the stack it had before.
 *
 * A brand new thread instead has `Context::rip` set to
 * `madrust_thread_trampoline` and `Context::rsp` pointing at a return address
 * placed at the top of its stack. The trampoline receives the thread index in
 * R12, enables interrupts (the switch runs with them disabled), runs the entry
 * function, and returns to that address once the entry function returns.
 */

    .section .text.context, "ax", @progbits
    .code64

    .global madrust_context_switch
    .type madrust_context_switch, @function
madrust_context_switch:
    movq %rsp, 0x00(%rdi)
    movq %rbx, 0x08(%rdi)
    movq %rbp, 0x10(%rdi)
    movq %r12, 0x18(%rdi)
    movq %r13, 0x20(%rdi)
    movq %r14, 0x28(%rdi)
    movq %r15, 0x30(%rdi)

    /*
     * Save the GS bases. The system call stub swaps them, so a thread that
     * blocks inside a call and a thread that was interrupted in ring 3 resume
     * with different values; the pair has to travel with the context.
     */
    movl $0xc0000101, %ecx      /* IA32_GS_BASE */
    rdmsr
    shlq $32, %rdx
    orq %rdx, %rax
    movq %rax, 0x40(%rdi)
    movl $0xc0000102, %ecx      /* IA32_KERNEL_GS_BASE */
    rdmsr
    shlq $32, %rdx
    orq %rdx, %rax
    movq %rax, 0x48(%rdi)

    /* Install the GS bases of the thread that is about to run. */
    movq 0x40(%rsi), %r8
    movl $0xc0000101, %ecx
    movl %r8d, %eax
    shrq $32, %r8
    movl %r8d, %edx
    wrmsr
    movq 0x48(%rsi), %r8
    movl $0xc0000102, %ecx
    movl %r8d, %eax
    shrq $32, %r8
    movl %r8d, %edx
    wrmsr

    movq 0x00(%rsi), %rsp
    movq 0x08(%rsi), %rbx
    movq 0x10(%rsi), %rbp
    movq 0x18(%rsi), %r12
    movq 0x20(%rsi), %r13
    movq 0x28(%rsi), %r14
    movq 0x30(%rsi), %r15

    pushq 0x38(%rsi)
    ret

    .global madrust_context_resume
    .type madrust_context_resume, @function
madrust_context_resume:
    ret

    .global madrust_thread_trampoline
    .type madrust_thread_trampoline, @function
madrust_thread_trampoline:
    sti
    movq %r12, %rdi
    call madrust_thread_start
    ret
