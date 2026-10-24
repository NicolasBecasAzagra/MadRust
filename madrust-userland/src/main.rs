// SPDX-License-Identifier: GPL-2.0-only
//! MadRust ring 3 test program, built against the standard library.
//!
//! This is a normal Rust program for the `x86_64-unknown-linux-musl` target,
//! statically linked. It shares no code with the kernel: every operation it
//! performs below `main` goes through the Linux system call interface that the
//! kernel implements in `madrust-kernel/src/user/syscall.rs`. Printing with
//! `println!` therefore reaches the serial console through `write`, and the
//! explicit `exit` leaves the program through `exit_group`.

fn main() {
    println!("Hello from MadRust userland");
    std::process::exit(0);
}
