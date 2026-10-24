// SPDX-License-Identifier: GPL-2.0-only
//! MadRust ring 3 test program.
//!
//! Freestanding and linked at `0x400000`. It makes two Linux system calls
//! directly with the `syscall` instruction: `write` of one greeting on the
//! kernel serial console, then `exit_group` with status zero. The kernel loads
//! this image from its own bytes, so no external userland or toolchain is
//! involved.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

use core::arch::asm;

/// Linux `write` system call number.
const SYS_WRITE: u64 = 1;
/// Linux `exit_group` system call number.
const SYS_EXIT_GROUP: u64 = 231;
/// File descriptor of the serial console.
const STDOUT: u64 = 1;

/// Text the program prints through `write`.
static GREETING: &[u8] = b"Hello from MadRust userland\r\n";

/// Issue a system call with three arguments.
///
/// # Safety
///
/// The caller must pass arguments the kernel accepts for `number`: a bad
/// pointer or count is reported as an error, it is never dereferenced here.
#[inline]
unsafe fn syscall3(number: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let result: u64;
    // SAFETY: `syscall` reads the registers named below and writes RAX, RCX
    // and R11. RCX and R11 are marked clobbered, the block does not touch the
    // stack, and the caller upholds the argument contract.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Issue the `exit_group` system call; it does not return.
fn exit_group(status: u64) -> ! {
    // SAFETY: `exit_group` takes one argument and never returns; the kernel
    // tears the program down instead of writing to memory.
    unsafe {
        syscall3(SYS_EXIT_GROUP, status, 0, 0);
    }
    loop {
        // SAFETY: `hlt` in ring 3 is privileged and faults; the kernel kills
        // the program before this is reached, but a loop keeps the signature
        // honest if it is ever reached.
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

/// Program entry point, called by the kernel through `iretq`.
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // SAFETY: the buffer is a static in this image and the length matches it.
    unsafe {
        syscall3(
            SYS_WRITE,
            STDOUT,
            GREETING.as_ptr() as u64,
            GREETING.len() as u64,
        );
    }
    exit_group(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    exit_group(1)
}
