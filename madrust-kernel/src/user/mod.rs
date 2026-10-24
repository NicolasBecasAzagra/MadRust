// SPDX-License-Identifier: GPL-2.0-only
//! User mode, system calls and the ELF64 program loader (milestone M4b).
//!
//! The program is a normal Rust program built against the standard library for
//! `x86_64-unknown-linux-musl`: a static position independent executable that
//! the loader maps at [`loader::LOAD_BASE`]. Everything it needs below `main`
//! arrives through the Linux system call interface in [`syscall`], including
//! the thread local storage pointer, the program break, anonymous mappings and
//! futex waits.
//!
//! ## One address space, two halves
//!
//! The kernel keeps running from the higher half and keeps its direct map, and
//! the user program is mapped into the *same* page tables: PML4 entry 0, the
//! low half, holds the program and its stack, while the existing kernel
//! entries are untouched. There is no per-process address space in this
//! milestone, no `CR3` switch on entry and no address space teardown on exit.
//! The program is the only user context and it runs until it exits.
//!
//! ## Flow
//!
//! [`load`] parses the embedded image with `madrust-elf`, maps every `PT_LOAD`
//! segment with the user bit set, copies the program header table into a user
//! page and builds the System V argument block on a fresh user stack.
//! [`run`] programs the `SYSCALL`/`SYSRET` MSRs and enters ring 3 with `iretq`.
//! The program leaves through the `exit_group` system call; the dispatcher
//! returns to the kernel frame saved by the assembly entry and [`run`] reports
//! the exit status.

mod errno;
mod fd;
mod futex;
mod loader;
mod paging;
mod stack;
mod syscall;
mod vm;

use core::arch::global_asm;

use madrust_arch::cpu;

pub use loader::LoadError;
pub use stack::StackError;

global_asm!(include_str!("syscall_entry.s"), options(att_syntax));

/// The user program, built before the kernel by the Makefile.
///
/// The path is inside the repository so that Cargo records the file as a
/// dependency of the kernel: rebuilding the userland changes this file and
/// therefore relinks the kernel. The boot log prints the size, so a stale
/// image is visible.
static EMBEDDED: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../madrust-userland/madrust-userland.elf"
));

/// Reason why the user program could not be prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserError {
    /// The ELF image, its segments or the program header copy is unusable.
    Load(LoadError),
    /// The initial stack could not be built.
    Stack(StackError),
}

/// A loaded user program with its initial stack.
pub struct Program {
    image: loader::Image,
    stack: u64,
}

/// Bytes of the embedded user program.
pub fn embedded_size() -> usize {
    EMBEDDED.len()
}

/// Load the embedded program into the user half and build its stack.
pub fn load() -> Result<Program, UserError> {
    let image = loader::load(EMBEDDED).map_err(UserError::Load)?;
    let stack = stack::build(&image, syscall::random_bytes()).map_err(UserError::Stack)?;
    Ok(Program { image, stack })
}

/// Entry point of a loaded program, with the load base applied.
pub fn entry(program: &Program) -> u64 {
    program.image.entry()
}

/// Address the program image was loaded at.
pub fn load_base(program: &Program) -> u64 {
    program.image.load_base()
}

/// Timer hook for the user subsystem.
///
/// The futex wait queue owns the timeouts, so the timer handler has to run its
/// expiry scan before the scheduler picks the next thread.
pub(crate) fn on_tick() {
    futex::on_tick();
}

/// Enable the system call model and run the program until it exits.
///
/// Returns the exit status the program passed to `exit_group`. Interrupts are
/// enabled again before returning, because the boot thread continues with the
/// timer driven checks.
pub fn run(program: &Program) -> u64 {
    syscall::set_break(program.image.break_address());
    syscall::init();

    // SAFETY: `load` mapped the entry point and the stack in the user half, the
    // IDT is installed, and the system call model was just enabled. The program
    // owns the transition: it returns only through the exit system call.
    let status = unsafe { syscall::enter(program.image.entry(), program.stack) };

    cpu::enable_interrupts();
    status
}
