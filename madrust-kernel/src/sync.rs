// SPDX-License-Identifier: GPL-2.0-only
//! Interrupt safe spinlock (milestone M3).
//!
//! The lock disables interrupts for as long as it is held, so the timer
//! handler cannot preempt a thread in the middle of a critical section and
//! then spin on the same lock forever. The classic sequence applies: save
//! RFLAGS, clear IF, acquire, release, restore RFLAGS.
//!
//! The owner field and the depth counter implement the recursion guard: a
//! thread that takes a lock it already holds is a bug in the caller, and the
//! kernel reports it instead of deadlocking. Recursion is never legal, so the
//! depth is normally one; it is kept because the guard has to distinguish a
//! nested take from a plain acquire.

use core::arch::asm;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use madrust_arch::cpu;

use crate::console::console;
use crate::sched;

/// Read RFLAGS and clear the interrupt flag.
///
/// Returns the previous flags so [`interrupt_restore`] can put the caller back
/// in the state it was in.
pub(crate) fn interrupt_save_disable() -> u64 {
    let flags: u64;
    // SAFETY: `pushfq` and `pop` only move RFLAGS through the stack, and
    // `cli` only clears IF. No memory is read or written outside the stack
    // word the two instructions push and pop.
    unsafe {
        asm!(
            "pushfq",
            "pop {}",
            "cli",
            out(reg) flags,
        );
    }
    flags
}

/// Write `flags` back to RFLAGS.
pub(crate) fn interrupt_restore(flags: u64) {
    // SAFETY: `flags` was read from RFLAGS by `interrupt_save_disable` or is
    // zero, and `popfq` only writes the flags register.
    unsafe {
        asm!("push {}", "popfq", in(reg) flags);
    }
}

/// Report a recursive acquire and stop the CPU.
#[cold]
fn recursive_violation(name: &str) -> ! {
    let mut uart = console();
    let _ = write!(
        uart,
        "\r\nMadRust: LOCK VIOLATION: {} re-acquired by thread token {}\r\n",
        name,
        sched::current_token()
    );
    uart.flush();
    cpu::halt_loop()
}

/// Spinlock that keeps interrupts disabled while it is held.
pub struct IrqSpinLock {
    name: &'static str,
    locked: AtomicBool,
    owner: AtomicU64,
    depth: AtomicUsize,
}

impl IrqSpinLock {
    /// Create an unlocked lock. `name` appears in the recursion report.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            locked: AtomicBool::new(false),
            owner: AtomicU64::new(0),
            depth: AtomicUsize::new(0),
        }
    }

    /// Take the lock, spinning until it is free.
    ///
    /// On return interrupts are disabled and stay disabled until the guard is
    /// dropped.
    pub fn lock(&self) -> IrqLockGuard<'_> {
        let flags = interrupt_save_disable();
        let token = sched::current_token();

        if self.owner.load(Ordering::Relaxed) == token {
            recursive_violation(self.name);
        }

        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        self.owner.store(token, Ordering::Relaxed);
        self.depth.store(1, Ordering::Relaxed);
        IrqLockGuard { lock: self, flags }
    }
}

/// Releases the lock and restores RFLAGS when it goes out of scope.
pub struct IrqLockGuard<'a> {
    lock: &'a IrqSpinLock,
    flags: u64,
}

impl Drop for IrqLockGuard<'_> {
    fn drop(&mut self) {
        let depth = self.lock.depth.load(Ordering::Relaxed);
        if depth > 1 {
            // Not reachable while recursion is rejected, but keeps the counter
            // meaningful if the policy ever allows it.
            self.lock.depth.store(depth - 1, Ordering::Relaxed);
        } else {
            self.lock.depth.store(0, Ordering::Relaxed);
            self.lock.owner.store(0, Ordering::Relaxed);
            self.lock.locked.store(false, Ordering::Release);
        }
        interrupt_restore(self.flags);
    }
}
