// SPDX-License-Identifier: GPL-2.0-only
//! Preemptive priority scheduler (milestone E3a).
//!
//! One CPU, one address space: threads differ only in their kernel stack, so a
//! switch is a register exchange and a stack pointer change. The run queue and
//! the current thread live in statics that are only touched with interrupts
//! disabled; there is no scheduler lock, because a lock held across a switch
//! would never be released by the thread that went to sleep.
//!
//! ## Policy
//!
//! Every thread carries a [`Priority`]. The scheduler runs the highest
//! priority that is runnable, and a lower priority never displaces a higher
//! one. All threads start at [`Priority::DEFAULT`], so the existing workloads
//! keep the round-robin behaviour they had. Two threads of the same priority
//! take turns: a switch pushes the thread that leaves the CPU to the back of
//! its priority group, and the queue keeps arrival order inside the group.
//!
//! A thread keeps the CPU for at most [`SLICE_TICKS`] timer interrupts. The
//! accounting tick stays at [`crate::pit::TICK_HZ`] (100 Hz), but the timer
//! interrupts ten times per tick, so a slice of two timer interrupts is two
//! milliseconds. The counter restarts whenever a thread is scheduled in.
//!
//! ## Wakeups
//!
//! A thread that makes another thread runnable preempts itself when the woken
//! thread has at least the priority of the running one ([`wake_thread`]). That
//! is what keeps the wakeup tail short: a futex wakeup does not wait for the
//! next tick. The check runs with interrupts disabled and the queue borrow has
//! already ended, because no lock and no borrow may survive the switch.
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
use madrust_sched::{Context, Priority, RunQueue, Slice, ThreadState};

use crate::sync;
use crate::thread;
use crate::thread::Start;

/// Length of one time slice in timer interrupts.
///
/// The PIT raises an interrupt every millisecond ([`crate::pit::IRQ_HZ`]), so
/// two interrupts are two milliseconds. A thread that never blocks keeps the
/// processor for at most this long; a shorter slice bounds the tail of a thread
/// that becomes runnable behind it.
pub const SLICE_TICKS: u64 = 2;

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

struct SliceCell(UnsafeCell<Slice>);

// SAFETY: the slice counter is only accessed with interrupts disabled, from the
// timer handler and from `schedule`, so the two cannot overlap.
unsafe impl Sync for SliceCell {}

static SLICE: SliceCell = SliceCell(UnsafeCell::new(Slice::new(SLICE_TICKS)));

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
    thread::set_kernel_stack_top(BOOT_THREAD, crate::gdt::interrupt_stack_top());
    CURRENT.store(0, Ordering::Release);
}

/// Create a thread at [`Priority::DEFAULT`] and queue it.
///
/// Called from the boot thread. Interrupts are disabled for the whole call so
/// that no timer tick can observe a half installed thread.
pub fn spawn(name: &'static str, entry: thread::EntryFn) -> Result<usize, thread::SpawnError> {
    spawn_with_priority(name, entry, Priority::DEFAULT)
}

/// Create a thread at an explicit priority and queue it.
///
/// The priority only changes the selection order; a new thread does not
/// preempt the caller, it runs at the next schedule.
pub fn spawn_with_priority(
    name: &'static str,
    entry: thread::EntryFn,
    priority: Priority,
) -> Result<usize, thread::SpawnError> {
    let flags = sync::interrupt_save_disable();
    let result = spawn_locked(name, entry, priority);
    sync::interrupt_restore(flags);
    result
}

fn spawn_locked(
    name: &'static str,
    entry: thread::EntryFn,
    priority: Priority,
) -> Result<usize, thread::SpawnError> {
    // A finished thread owns no stack that is still in use, so its slot can be
    // recycled before a new thread claims it.
    thread::reclaim_finished();
    let id = thread::spawn(name, entry, priority)?;

    // SAFETY: interrupts are disabled by `spawn`, so this is the only access
    // to the queue.
    let queue = unsafe { &mut *QUEUE.0.get() };
    if queue.push(id, priority).is_err() {
        thread::remove(id);
        return Err(thread::SpawnError::NoFreeSlot);
    }
    Ok(id)
}

/// Create a user thread from the `clone` system call.
///
/// The caller runs inside the system call with interrupts masked, which is the
/// critical section `spawn` needs. The child is queued and picked up by the
/// next schedule, exactly like a kernel thread.
pub fn spawn_user(
    name: &'static str,
    start: thread::UserStart,
    fs_base: u64,
    clear_child_tid: u64,
    kernel_gs_base: u64,
) -> Result<usize, thread::SpawnError> {
    thread::reclaim_finished();
    let id = thread::spawn_user(name, start, fs_base, clear_child_tid, kernel_gs_base)?;

    // SAFETY: interrupts are masked for the whole system call, so this is the
    // only access to the queue.
    let queue = unsafe { &mut *QUEUE.0.get() };
    if queue.push(id, Priority::DEFAULT).is_err() {
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
/// thread identifier plus one. The woken thread goes back to the run queue at
/// its own priority. When it is at least as important as the running thread,
/// the caller gives up the CPU immediately instead of waiting for the next
/// tick: that is the short wakeup tail this milestone measures.
///
/// The queue borrow and every other borrow end before [`schedule`] runs, and
/// interrupts stay disabled across the switch; no lock is held.
pub fn wake_thread(token: u64) {
    let Some(id) = token.checked_sub(1) else {
        return;
    };
    let Ok(id) = usize::try_from(id) else {
        return;
    };
    let flags = sync::interrupt_save_disable();
    let mut woken = None;
    {
        let priority = thread::priority(id).unwrap_or(Priority::DEFAULT);
        if thread::state(id) == Some(ThreadState::Blocked) {
            thread::set_state(id, ThreadState::Runnable);
            // SAFETY: interrupts are disabled, so this is the only access to
            // the queue and the borrow ends before any switch could run.
            let queue = unsafe { &mut *QUEUE.0.get() };
            if queue.push(id, priority).is_ok() {
                woken = Some(priority);
            }
        }
    }
    if woken.is_some_and(preempts_current) {
        schedule();
    }
    sync::interrupt_restore(flags);
}

/// `true` when a woken thread at `priority` must displace the running thread.
fn preempts_current(priority: Priority) -> bool {
    let Some(current) = current_id() else {
        return false;
    };
    if !thread::state(current).is_some_and(ThreadState::is_runnable) {
        return false;
    }
    priority >= thread::priority(current).unwrap_or(Priority::DEFAULT)
}

/// Timer hook: preempt the running thread when its slice has expired.
///
/// Runs inside the timer interrupt handler, after the EOI, with interrupts
/// disabled.
pub fn on_tick() {
    // SAFETY: the timer handler runs with interrupts disabled, so this is the
    // only access to the slice counter; the borrow ends before `user::on_tick`
    // and `schedule`, both of which may switch threads.
    let expired = unsafe { (*SLICE.0.get()).tick() };
    // Expired futex waiters become runnable before the next pick, so a thread
    // that timed out is not left waiting for a wakeup that will never come.
    crate::user::on_tick();
    if expired {
        schedule();
    }
}

/// Pick the next thread and switch to it.
///
/// Must run with interrupts disabled. If no thread can displace the running
/// one, it keeps the CPU; a finished thread with nobody left to run stops the
/// CPU instead of spinning in a stale context.
pub fn schedule() {
    let Some(current) = current_id() else {
        return;
    };
    let current_state = thread::state(current);
    // A thread that can run again is only displaced by an entry of at least its
    // own priority. A blocked or finished thread lets any priority run.
    let current_slot = if current_state.is_some_and(ThreadState::is_runnable) {
        Some((
            current,
            thread::priority(current).unwrap_or(Priority::DEFAULT),
        ))
    } else {
        None
    };

    let next = {
        // SAFETY: interrupts are disabled, so this is the only access to the
        // queue for the duration of the block.
        let queue = unsafe { &mut *QUEUE.0.get() };
        match queue.pick_next(current_slot) {
            Some(next) if next != current => {
                // The thread that leaves the CPU goes to the back of its
                // priority group unless it cannot run again.
                if let Some((_, priority)) = current_slot {
                    thread::set_state(current, ThreadState::Runnable);
                    let _ = queue.push(current, priority);
                }
                queue.remove(next);
                thread::set_state(next, ThreadState::Running);
                CURRENT.store(next, Ordering::Release);
                // SAFETY: interrupts are disabled and the counter is not
                // borrowed anywhere, so this is the only access.
                unsafe { (*SLICE.0.get()).reset() };
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

        // The CPU installs `rsp0` on the next interrupt from ring 3 and the
        // system call stub loads the same field, so it must name the stack of
        // the thread that is about to run. `IA32_FS_BASE` is per thread too,
        // because each user thread has its own thread local storage block.
        if let Some(top) = thread::kernel_stack_top(next) {
            crate::gdt::set_kernel_stack(top);
        }
        if let Some(base) = thread::fs_base(next) {
            crate::user::load_fs_base(base);
        }
        // The user stack slot is per CPU, but a thread that blocked inside a
        // system call must return to its own stack; give it its saved value.
        crate::user::install_user_stack(thread::user_stack(next));

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
/// `id` arrives in R12 and names the thread table slot. A kernel thread calls
/// its entry function and retires when it returns; a user thread enters ring 3
/// and never comes back through this path.
#[no_mangle]
pub extern "C" fn madrust_thread_start(id: usize) {
    match thread::entry(id) {
        Some(entry) if entry.id == id => match entry.start {
            Start::Kernel { function, name } => function(name),
            Start::User(start) => crate::user::enter_thread(start),
        },
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

/// Retire the calling thread after its exit system call.
///
/// The thread is finished and never scheduled again. Its kernel stack is
/// released the next time another thread creates a thread, which is safe
/// because the switch away is what stops using it.
pub fn retire_current() -> ! {
    let _flags = sync::interrupt_save_disable();
    if let Some(current) = current_id() {
        thread::set_state(current, ThreadState::Finished);
        FINISHED.fetch_add(1, Ordering::Release);
    }
    loop {
        schedule();
    }
}
