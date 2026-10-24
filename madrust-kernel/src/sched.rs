// SPDX-License-Identifier: GPL-2.0-only
//! Preemptive round-robin scheduler (milestone M3).
//!
//! One CPU, one address space: threads differ only in their kernel stack, so a
//! switch is a register exchange and a stack pointer change. The run queue and
//! the current thread live in statics that are only touched with interrupts
//! disabled; there is no scheduler lock, because a lock held across a switch
//! would never be released by the thread that went to sleep.
//!
//! Preemption reuses the interrupt frame instead of copying it. When the timer
//! handler calls [`schedule`] it is running on the stack of the thread it
//! interrupted, above that thread's [`InterruptFrame`](crate::idt::InterruptFrame).
//! The switch saves the handler stack pointer into the thread context, so the
//! frame stays exactly where the CPU and the stubs put it. When the thread is
//! picked again, `madrust_context_resume` returns to `schedule`, the handler
//! returns, and `iretq` restores the interrupted registers. Nothing is copied
//! and no interrupt is lost.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use madrust_arch::cpu;
use madrust_sched::{Context, RunQueue, ThreadState};

use crate::sync;
use crate::thread;

/// Number of timer ticks between two preemptions.
///
/// The PIT runs at 100 Hz, so one tick is a 10 ms time slice. One tick keeps
/// the example threads in step: each thread prints one line per slice, which
/// makes the interleaving of the serial log deterministic.
pub const PREEMPT_INTERVAL_TICKS: u64 = 1;

/// Sentinel stored in [`CURRENT`] before [`init`] runs.
const NO_THREAD: usize = usize::MAX;

/// Slot of the boot thread in the thread table.
///
/// It is the thread that enters the user program. It never sits in the run
/// queue, because it keeps its stack while it serves a blocking system call.
const BOOT_THREAD: usize = 0;

static CURRENT: AtomicUsize = AtomicUsize::new(NO_THREAD);
static FINISHED: AtomicUsize = AtomicUsize::new(0);

struct QueueCell(UnsafeCell<RunQueue>);

// SAFETY: the queue is only accessed with interrupts disabled, so the timer
// handler and the thread path cannot overlap.
unsafe impl Sync for QueueCell {}

static QUEUE: QueueCell = QueueCell(UnsafeCell::new(RunQueue::new()));

extern "C" {
    /// Save the first context into RDI and load the second from RSI.
    fn madrust_context_switch(current: *mut Context, next: *const Context);
    /// One `ret` that resumes a switched-out thread on its own stack.
    fn madrust_context_resume();
}

/// Register the boot context and start with an empty run queue.
///
/// Called once during boot, before interrupts are enabled.
pub fn init() {
    thread::install_boot();
    CURRENT.store(0, Ordering::Release);
}

/// Create a thread and put it at the back of the run queue.
///
/// Called from the boot thread. Interrupts are disabled for the whole call so
/// that no timer tick can observe a half installed thread.
pub fn spawn(name: &'static str, entry: thread::EntryFn) -> Result<usize, thread::SpawnError> {
    let flags = sync::interrupt_save_disable();
    let result = spawn_locked(name, entry);
    sync::interrupt_restore(flags);
    result
}

fn spawn_locked(name: &'static str, entry: thread::EntryFn) -> Result<usize, thread::SpawnError> {
    let id = thread::spawn(name, entry)?;

    // SAFETY: interrupts are disabled by `spawn`, so this is the only access
    // to the queue.
    let queue = unsafe { &mut *QUEUE.0.get() };
    if queue.push(id).is_err() {
        thread::remove(id);
        return Err(thread::SpawnError::NoFreeSlot);
    }
    Ok(id)
}

/// Identifier of the running thread, or `None` before [`init`].
pub fn current_id() -> Option<usize> {
    let current = CURRENT.load(Ordering::Relaxed);
    if current == NO_THREAD {
        None
    } else {
        Some(current)
    }
}

/// Stable token of the running execution context.
///
/// The token is the thread identifier plus one, so it is never zero: zero is
/// the "lock is free" value of [`crate::sync::IrqSpinLock`]. Before the
/// scheduler starts, the single-threaded boot context uses token one.
pub fn current_token() -> u64 {
    match current_id() {
        Some(id) => id as u64 + 1,
        None => 1,
    }
}

/// Number of threads that have returned from their entry function.
pub fn finished_threads() -> usize {
    FINISHED.load(Ordering::Acquire)
}

/// Give up the CPU if another thread is runnable.
///
/// The caller keeps its `Runnable` state and goes to the back of the queue, so
/// a voluntary yield follows the same round-robin order as a preemption.
pub fn yield_now() {
    let flags = sync::interrupt_save_disable();
    schedule();
    sync::interrupt_restore(flags);
}

/// Remove the running thread from the run queue and mark it blocked.
///
/// Used by the futex wait: the caller registers its waiter first, then blocks,
/// so a wakeup that happens in between is not lost. The thread keeps its
/// stack, so the caller continues in place once [`wake_thread`] runs.
pub fn block_current() {
    let flags = sync::interrupt_save_disable();
    if let Some(current) = current_id() {
        // SAFETY: interrupts are disabled, so this is the only access to the
        // queue and the borrow ends before interrupts are restored.
        let queue = unsafe { &mut *QUEUE.0.get() };
        queue.remove(current);
        if thread::state(current).is_some_and(ThreadState::is_runnable) {
            thread::set_state(current, ThreadState::Blocked);
        }
    }
    sync::interrupt_restore(flags);
}

/// Make a blocked thread runnable again.
///
/// `token` is the value [`current_token`] returned to the waiter, that is the
/// thread identifier plus one. The boot thread only changes state: it has
/// never been switched out, so its context is the one it is running on. Every
/// other thread goes back to the run queue and is picked up by the next
/// schedule.
pub fn wake_thread(token: u64) {
    let Some(id) = token.checked_sub(1) else {
        return;
    };
    let Ok(id) = usize::try_from(id) else {
        return;
    };
    let flags = sync::interrupt_save_disable();
    if thread::state(id) == Some(ThreadState::Blocked) {
        thread::set_state(id, ThreadState::Runnable);
        if id != BOOT_THREAD {
            // SAFETY: interrupts are disabled, so this is the only access to
            // the queue and the borrow ends before interrupts are restored.
            let _ = unsafe { &mut *QUEUE.0.get() }.push(id);
        }
    }
    sync::interrupt_restore(flags);
}

/// Timer hook: preempt the running thread every [`PREEMPT_INTERVAL_TICKS`].
///
/// Runs inside the timer interrupt handler, after the EOI, with interrupts
/// disabled.
pub fn on_tick() {
    // Expired futex waiters become runnable before the next pick, so a thread
    // that timed out is not left waiting for a wakeup that will never come.
    crate::user::on_tick();
    if crate::pit::ticks().is_multiple_of(PREEMPT_INTERVAL_TICKS) {
        schedule();
    }
}

/// Pick the next thread and switch to it.
///
/// Must run with interrupts disabled. If no other thread is runnable, the
/// current thread keeps the CPU; a finished thread with nobody left to run
/// stops the CPU instead of spinning in a stale context.
pub fn schedule() {
    let Some(current) = current_id() else {
        return;
    };

    let next = {
        // SAFETY: interrupts are disabled, so this is the only access to the
        // queue for the duration of the block.
        let queue = unsafe { &mut *QUEUE.0.get() };
        match queue.pick_next(Some(current)) {
            Some(next) if next != current => {
                // The thread that leaves the CPU goes to the back of the queue
                // unless it cannot run again.
                if thread::state(current).is_some_and(ThreadState::is_runnable) {
                    thread::set_state(current, ThreadState::Runnable);
                    let _ = queue.push(current);
                }
                queue.remove(next);
                thread::set_state(next, ThreadState::Running);
                CURRENT.store(next, Ordering::Release);
                Some(next)
            }
            _ => None,
        }
    };

    if let Some(next) = next {
        let (Some(old_context), Some(new_context)) =
            (thread::context_mut_ptr(current), thread::context_ptr(next))
        else {
            lost_thread(current, next);
        };

        // SAFETY: both pointers refer to contexts in the static thread table,
        // interrupts are disabled, and the switch restores the callee saved
        // registers it overwrites. The resume address is written first because
        // the assembly does not save RIP: `madrust_context_resume` pops the
        // return address of this call and continues in this function.
        unsafe {
            (*old_context).rip = madrust_context_resume as *const () as usize as u64;
            madrust_context_switch(old_context, new_context);
        }
        // Control returns here only when another thread switches back to this
        // one; that thread already made this thread Running and current.
    } else if thread::state(current) == Some(ThreadState::Finished) {
        // The last thread retired. There is nothing left to schedule.
        cpu::enable_interrupts();
        cpu::halt_loop();
    }
}

/// Stop with a clear report when the table and the current identifier disagree.
#[cold]
fn lost_thread(current: usize, next: usize) -> ! {
    crate::kprintln!(
        "MadRust: scheduler FAILED: context missing for thread {} -> {}",
        current,
        next
    );
    crate::console::console().flush();
    crate::debug_exit::exit(crate::debug_exit::EXIT_SCHEDULER_FAILED)
}

/// Entry point a new thread reaches through the assembly trampoline.
///
/// `id` arrives in R12 and names the thread table slot. The function returns
/// when the thread entry returns; the trampoline then retires the thread.
#[no_mangle]
pub extern "C" fn madrust_thread_start(id: usize) {
    match thread::entry(id) {
        Some(entry) if entry.id == id => (entry.function)(entry.name),
        _ => {
            crate::kprintln!("MadRust: scheduler FAILED: thread {} does not exist", id);
            crate::console::console().flush();
            cpu::halt_loop();
        }
    }
}

/// Retirement path the assembly trampoline returns into.
///
/// Marks the thread finished, counts it and schedules; the finished thread is
/// never selected again, so this never returns.
#[no_mangle]
pub extern "C" fn madrust_thread_completed() -> ! {
    let _flags = sync::interrupt_save_disable();
    if let Some(current) = current_id() {
        thread::set_state(current, ThreadState::Finished);
        FINISHED.fetch_add(1, Ordering::Release);
    }
    loop {
        schedule();
    }
}
