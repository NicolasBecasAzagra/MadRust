// SPDX-License-Identifier: GPL-2.0-only
//! Futex wait queue.
//!
//! A `FUTEX_WAIT` blocks the calling thread until another thread stores a new
//! value at the futex word and calls `FUTEX_WAKE`, or until the timeout
//! expires. The queue is a fixed size array, because the wait path must not
//! allocate: a thread that is going to sleep cannot be holding the heap lock.
//!
//! Every entry is keyed by the address of the futex word and by the identifier
//! of the waiting thread. The producer only needs the address: `FUTEX_WAKE`
//! wakes up to the requested number of threads that wait on that word.
//!
//! ## Locking
//!
//! The table is protected by the interrupt flag, exactly like the run queue:
//! the check of the futex word, the insertion of the waiter and the change of
//! the thread state happen in one critical section, so a `FUTEX_WAKE` on the
//! same CPU cannot observe a half installed waiter. The sleeping loop runs
//! with interrupts enabled again, so the timer keeps ticking and the timeout
//! can expire.

use core::cell::UnsafeCell;

use madrust_arch::cpu;

use crate::pit;
use crate::sched;
use crate::sync;

use super::errno::{EAGAIN, ETIMEDOUT};

/// Maximum number of threads that can wait on a futex at the same time.
const MAX_WAITERS: usize = 8;

/// One blocked thread.
#[derive(Clone, Copy)]
struct Waiter {
    /// Address of the futex word.
    address: u64,
    /// Thread token reported by [`crate::sched::current_token`].
    thread: u64,
    /// Absolute timer tick at which the wait gives up, zero for no timeout.
    deadline: u64,
    /// The timer reached `deadline` and the waiter must report `ETIMEDOUT`.
    expired: bool,
    /// A slot that is in use.
    active: bool,
}

const EMPTY: Waiter = Waiter {
    address: 0,
    thread: 0,
    deadline: 0,
    expired: false,
    active: false,
};

struct Table(UnsafeCell<[Waiter; MAX_WAITERS]>);

// SAFETY: the table is only reached through the helpers below, each of which
// runs with interrupts disabled for the whole mutation.
unsafe impl Sync for Table {}

static TABLE: Table = Table(UnsafeCell::new([EMPTY; MAX_WAITERS]));

/// Exclusive view of the table.
///
/// # Safety
///
/// The caller must have interrupts disabled and must not keep the borrow while
/// it blocks.
unsafe fn table() -> &'static mut [Waiter; MAX_WAITERS] {
    // SAFETY: the caller upholds the exclusivity rule stated above.
    unsafe { &mut *TABLE.0.get() }
}

/// `FUTEX_WAIT`: block until the futex word changes or the timeout expires.
///
/// `value` is the word the caller just read at `address`; the wait fails with
/// `EAGAIN` when the word no longer matches `expected`, which is what makes the
/// operation race free.
///
/// `timeout` is an optional duration in nanoseconds, already converted from
/// the `struct timespec` the program passed.
pub fn wait(address: u64, value: u32, expected: u32, timeout: Option<u64>) -> i64 {
    if value != expected {
        return EAGAIN;
    }

    let flags = sync::interrupt_save_disable();

    let thread = sched::current_token();
    let deadline = match timeout {
        Some(nanoseconds) => pit::ticks() + ticks_for(nanoseconds),
        None => 0,
    };

    // SAFETY: interrupts are disabled, so this is the only access to the
    // table, and no borrow survives the end of this block.
    let slot = {
        let waiters = unsafe { table() };
        match waiters.iter().position(|waiter| !waiter.active) {
            Some(index) => {
                waiters[index] = Waiter {
                    address,
                    thread,
                    deadline,
                    expired: false,
                    active: true,
                };
                Some(index)
            }
            None => None,
        }
    };
    if slot.is_none() {
        sync::interrupt_restore(flags);
        return EAGAIN;
    }

    sched::block_current();
    // Let a runnable thread take the CPU immediately instead of waiting for
    // the next timer tick: a wakeup that arrived before the block is already
    // queued, and a waker that blocks right after its `FUTEX_WAKE` should hand
    // the CPU to the waiter without a full time slice of delay.
    sched::yield_now();
    sync::interrupt_restore(flags);

    // A system call arrives with interrupts masked. The wait needs the timer,
    // both to expire the timeout and to give the `hlt` below an exit, so the
    // flag is set for the whole loop. The `sysretq` that ends the call restores
    // the user's own flags.
    cpu::enable_interrupts();

    loop {
        let flags = sync::interrupt_save_disable();
        // SAFETY: interrupts are disabled for the whole check.
        let waiters = unsafe { table() };
        let index = waiters
            .iter()
            .position(|waiter| waiter.active && waiter.thread == thread);
        let Some(index) = index else {
            // `FUTEX_WAKE` removed the entry: the wakeup succeeded.
            sync::interrupt_restore(flags);
            return 0;
        };
        let expired = waiters[index].expired;
        // A store to the futex word without a wake is not supposed to end the
        // wait on Linux, but treating it as a wakeup is harmless: the futex
        // contract already allows a spurious return, and it lets a program that
        // never calls `FUTEX_WAKE` make progress.
        let changed = read_word(address) != expected;
        if expired || changed {
            waiters[index] = EMPTY;
            sync::interrupt_restore(flags);
            return if expired { ETIMEDOUT } else { 0 };
        }
        sync::interrupt_restore(flags);

        // Sleep until the next timer tick or a wakeup; the loop rechecks.
        cpu::hlt();
    }
}

/// Timer hook: wake the waiters whose timeout has expired.
///
/// Runs from the timer interrupt with interrupts masked. The waiter only
/// marks the entry, because a thread that is off the CPU cannot notice its own
/// deadline: the tick has to make it runnable, and the wait loop reports
/// `ETIMEDOUT` when it runs again.
pub fn on_tick() {
    let flags = sync::interrupt_save_disable();
    // SAFETY: interrupts are disabled for the whole scan, so this is the only
    // access to the table and the borrow ends before the flags are restored.
    let waiters = unsafe { table() };
    for waiter in waiters.iter_mut() {
        if waiter.active
            && !waiter.expired
            && waiter.deadline != 0
            && pit::ticks() >= waiter.deadline
        {
            waiter.expired = true;
            sched::wake_thread(waiter.thread);
        }
    }
    sync::interrupt_restore(flags);
}

/// `FUTEX_WAKE`: make up to `count` waiters on `address` runnable.
///
/// Returns the number of threads that were woken.
pub fn wake(address: u64, count: u64) -> i64 {
    let flags = sync::interrupt_save_disable();
    let mut woken = 0u64;

    // SAFETY: interrupts are disabled for the whole mutation and the borrow
    // ends before the loop returns.
    let waiters = unsafe { table() };
    for waiter in waiters.iter_mut() {
        if woken >= count {
            break;
        }
        if !waiter.active || waiter.address != address {
            continue;
        }
        let thread = waiter.thread;
        *waiter = EMPTY;
        sched::wake_thread(thread);
        woken += 1;
    }

    sync::interrupt_restore(flags);
    i64::try_from(woken).unwrap_or(i64::MAX)
}

/// Read the futex word.
///
/// The caller checked that `address` is a mapped user address before it
/// entered the wait, and the user program cannot run while the system call is
/// in progress, so the page is still there.
fn read_word(address: u64) -> u32 {
    // SAFETY: the address was validated as mapped user memory by the system
    // call dispatcher and stays mapped for the whole wait. The read is
    // volatile, so every pass of the wait loop sees a new store.
    unsafe { (address as *const u32).read_volatile() }
}

/// Timer ticks that cover `nanoseconds`, rounded up.
///
/// The PIT resolution is 10 ms, so a shorter timeout still waits for one tick
/// instead of returning immediately: a zero timeout has already been handled
/// by the caller as "do not wait".
fn ticks_for(nanoseconds: u64) -> u64 {
    let tick = 1_000_000_000 / pit::TICK_HZ;
    let ticks = nanoseconds / tick;
    if nanoseconds.is_multiple_of(tick) {
        ticks.max(1)
    } else {
        ticks + 1
    }
}
