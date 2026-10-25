// SPDX-License-Identifier: GPL-2.0-only
//! Preemptive priority scheduler with one run queue per processor (E3b).
//!
//! One address space, several processors: threads differ only in their kernel
//! stack, so a switch is a register exchange and a stack pointer change. Each
//! processor owns its run queue, its current thread, its slice counter and its
//! finished counter in [`crate::percpu::PerCpu`]. The timer handler of a
//! processor only touches that block, so the scheduling path needs no global
//! lock. The only cross processor operation is a wakeup that pushes a thread
//! into another processor's queue; it takes that queue's own lock and sends a
//! reschedule interrupt.
//!
//! ## Policy
//!
//! Every thread carries a [`Priority`]. The scheduler runs the highest
//! priority that is runnable, and a lower priority never displaces a higher
//! one. All threads start at [`Priority::DEFAULT`], so the existing workloads
//! keep the round-robin behaviour they had. Two threads of the same priority
//! take turns: a switch pushes the thread that leaves the processor to the back
//! of its priority group, and the queue keeps arrival order inside the group.
//!
//! A thread keeps the processor for at most [`SLICE_TICKS`] timer interrupts.
//! The local APIC timer raises an interrupt every millisecond on every
//! processor, so a slice of two interrupts is two milliseconds.
//!
//! ## Wakeups
//!
//! A thread that makes another thread runnable preempts itself when the woken
//! thread has at least the priority of the running one and the thread belongs
//! to the same processor. When the woken thread belongs to another processor,
//! the thread is queued there and that processor receives a reschedule
//! interrupt, which also wakes it from its idle `hlt`.
//!
//! Preemption reuses the interrupt frame instead of copying it. When the timer
//! handler calls [`schedule`] it is running on the stack of the thread it
//! interrupted, above that thread's [`InterruptFrame`](crate::idt::InterruptFrame).
//! The switch saves the handler stack pointer into the thread context, so the
//! frame stays exactly where the CPU and the stubs put it. When the thread is
//! picked again, `madrust_context_resume` returns to `schedule`, the handler
//! returns, and `iretq` restores the interrupted registers.

use core::sync::atomic::Ordering;

use madrust_arch::cpu;
use madrust_sched::{Context, Priority, RunQueue, ThreadState};

use crate::percpu::{self, NO_THREAD};
use crate::sync;
use crate::thread;
use crate::thread::Start;

/// Length of one time slice in timer interrupts.
///
/// The local APIC timer raises an interrupt every millisecond, so two
/// interrupts are two milliseconds. A thread that never blocks keeps the
/// processor for at most this long; a shorter slice bounds the tail of a thread
/// that becomes runnable behind it.
pub const SLICE_TICKS: u64 = 2;

extern "C" {
    /// Save the first context into RDI and load the second from RSI.
    fn madrust_context_switch(current: *mut Context, next: *const Context);
    /// One `ret` that resumes a switched-out thread on its own stack.
    fn madrust_context_resume();
}

/// Slot of the boot thread in the thread table.
///
/// It is the thread that enters the user program. It never sits in the run
/// queue for long, because it serves the program until it exits.
const BOOT_THREAD: usize = 0;

/// Run the current processor's scheduler.
///
/// Called once on the boot processor, before interrupts are enabled. The
/// application processors reach their state through [`ap_start`].
pub fn init() {
    let processor = percpu::this();
    thread::install_boot();
    thread::set_kernel_stack_top(
        BOOT_THREAD,
        crate::gdt::interrupt_stack_top(processor.index),
    );
    thread::set_cpu(BOOT_THREAD, processor.index);
    thread::set_on_cpu(BOOT_THREAD, Some(processor.index));
    processor.current.store(BOOT_THREAD, Ordering::Release);
    processor.idle_thread.store(BOOT_THREAD, Ordering::Release);
    processor.online.store(true, Ordering::Release);
}

/// Enter the idle loop of an application processor.
///
/// The thread table slot and its stack were installed by the boot processor
/// before the start up interrupt. The idle thread is never queued: it is the
/// context the processor runs when it has nothing else to do, and a switch to
/// real work pushes it back into the run queue like any other thread.
pub fn ap_start(index: usize) -> ! {
    let Some(processor) = percpu::get(index) else {
        cpu::halt_loop();
    };
    let idle = processor.idle_thread.load(Ordering::Acquire);
    if idle == NO_THREAD {
        cpu::halt_loop();
    }
    thread::set_on_cpu(idle, Some(index));
    if let Some(top) = thread::kernel_stack_top(idle) {
        crate::gdt::set_kernel_stack(top);
    }
    processor.current.store(idle, Ordering::Release);
    processor.online.store(true, Ordering::Release);

    cpu::enable_interrupts();
    loop {
        cpu::hlt();
    }
}

/// Create a thread at [`Priority::DEFAULT`] on the calling processor.
pub fn spawn(name: &'static str, entry: thread::EntryFn) -> Result<usize, thread::SpawnError> {
    spawn_with_priority(name, entry, Priority::DEFAULT)
}

/// Create a thread at an explicit priority on the calling processor.
pub fn spawn_with_priority(
    name: &'static str,
    entry: thread::EntryFn,
    priority: Priority,
) -> Result<usize, thread::SpawnError> {
    let flags = sync::interrupt_save_disable();
    let result = spawn_locked(name, entry, priority, percpu::this().index);
    sync::interrupt_restore(flags);
    result
}

/// Create a thread on a named processor.
///
/// A thread placed on another processor is queued there and the processor
/// receives a reschedule interrupt, so it does not wait for its next timer
/// tick when it is idle.
pub fn spawn_on_cpu(
    index: usize,
    name: &'static str,
    entry: thread::EntryFn,
    priority: Priority,
) -> Result<usize, thread::SpawnError> {
    let flags = sync::interrupt_save_disable();
    let target = if percpu::get(index).is_some_and(|cpu| cpu.online.load(Ordering::Acquire)) {
        index
    } else {
        percpu::this().index
    };
    let result = spawn_locked(name, entry, priority, target);
    sync::interrupt_restore(flags);
    result
}

fn spawn_locked(
    name: &'static str,
    entry: thread::EntryFn,
    priority: Priority,
    target: usize,
) -> Result<usize, thread::SpawnError> {
    // A finished thread owns no stack that is still in use once its processor
    // has switched away from it, so its slot can be recycled.
    thread::reclaim_finished();
    let id = thread::spawn(name, entry, priority)?;
    thread::set_cpu(id, target);
    if !push_to(target, id, priority) {
        thread::remove(id);
        return Err(thread::SpawnError::NoFreeSlot);
    }
    if target != percpu::this().index {
        reschedule(target);
    }
    Ok(id)
}

/// Create a user thread from the `clone` system call.
///
/// The caller runs inside the system call with interrupts masked, which is the
/// critical section `spawn` needs. The child is queued on the same processor
/// as its parent and picked up by the next schedule.
pub fn spawn_user(
    name: &'static str,
    start: thread::UserStart,
    fs_base: u64,
    clear_child_tid: u64,
    kernel_gs_base: u64,
) -> Result<usize, thread::SpawnError> {
    thread::reclaim_finished();
    let id = thread::spawn_user(name, start, fs_base, clear_child_tid, kernel_gs_base)?;
    let target = percpu::this().index;
    thread::set_cpu(id, target);
    if !push_to(target, id, Priority::DEFAULT) {
        thread::remove(id);
        return Err(thread::SpawnError::NoFreeSlot);
    }
    Ok(id)
}

/// Push `id` into the run queue of `target`.
fn push_to(target: usize, id: usize, priority: Priority) -> bool {
    let Some(processor) = percpu::get(target) else {
        return false;
    };
    let _guard = processor.lock_queue();
    // SAFETY: the guard owns the queue lock, and no borrow survives a switch.
    let queue = unsafe { processor.queue_mut() };
    queue.push(id, priority).is_ok()
}

/// Remove `id` from the run queue of `target`.
fn remove_from(target: usize, id: usize) -> bool {
    let Some(processor) = percpu::get(target) else {
        return false;
    };
    let _guard = processor.lock_queue();
    // SAFETY: the guard owns the queue lock.
    unsafe { processor.queue_mut() }.remove(id)
}

/// Run queue that currently holds `id`, for diagnostics.
pub fn queue_of(id: usize) -> Option<usize> {
    for index in 0..percpu::MAX_CPUS {
        let Some(processor) = percpu::get(index) else {
            continue;
        };
        let _guard = processor.lock_queue();
        // SAFETY: the guard owns the queue lock and the borrow ends before the
        // guard is dropped.
        if unsafe { processor.queue_mut() }.contains(id) {
            return Some(index);
        }
    }
    None
}

/// Send the reschedule interrupt to `target`.
fn reschedule(target: usize) {
    // The queue push must be visible to the target before the interrupt lands,
    // so the fence orders it ahead of the ICR write.
    core::sync::atomic::fence(Ordering::SeqCst);
    let Some(processor) = percpu::get(target) else {
        return;
    };
    let destination = processor.apic_id.load(Ordering::Acquire);
    crate::lapic::send_ipi(destination, crate::vectors::IPI_RESCHEDULE_VECTOR);
}

/// Identifier of the running thread on this processor, or `None`.
pub fn current_id() -> Option<usize> {
    let current = percpu::this().current.load(Ordering::Acquire);
    if current == NO_THREAD {
        None
    } else {
        Some(current)
    }
}

/// Stable token of the running execution context.
///
/// The token is unique across processors: the high half names the processor,
/// the low half the thread identifier plus one. Zero is the "lock is free"
/// value of [`crate::sync::IrqSpinLock`].
pub fn current_token() -> u64 {
    let processor = percpu::this().index as u64;
    match current_id() {
        Some(id) => (processor << 32) | (id as u64 + 1),
        None => (processor << 32) | 1,
    }
}

/// Number of threads that have returned from their entry function.
pub fn finished_threads() -> usize {
    let mut total = 0;
    for index in 0..percpu::MAX_CPUS {
        if let Some(processor) = percpu::get(index) {
            total += processor.finished.load(Ordering::Acquire);
        }
    }
    total
}

/// Give up the processor if another thread is runnable.
pub fn yield_now() {
    let flags = sync::interrupt_save_disable();
    schedule();
    sync::interrupt_restore(flags);
}

/// Remove the running thread from the run queue and mark it blocked.
///
/// The thread keeps its stack and its processor identifier, so the wakeup
/// lands on the processor where it went to sleep.
pub fn block_current() {
    let flags = sync::interrupt_save_disable();
    if let Some(current) = current_id() {
        thread::set_cpu(current, percpu::this().index);
        remove_from(percpu::this().index, current);
        if thread::state(current).is_some_and(ThreadState::is_runnable) {
            thread::set_state(current, ThreadState::Blocked);
        }
    }
    sync::interrupt_restore(flags);
}

/// Make a blocked thread runnable again.
///
/// `token` is the value [`current_token`] returned to the waiter. The thread is
/// queued on the processor where it blocked. When that is the calling
/// processor and the woken thread is at least as important, the caller gives up
/// the processor immediately instead of waiting for the next tick: that is the
/// short wakeup tail this milestone measures. A different processor receives a
/// reschedule interrupt instead.
pub fn wake_thread(token: u64) {
    let Some(id) = token.checked_sub(1) else {
        return;
    };
    let Ok(id) = usize::try_from(id & 0xffff_ffff) else {
        return;
    };
    let here = percpu::this().index;
    let flags = sync::interrupt_save_disable();
    let mut woken = None;
    let mut remote = None;
    // The claim is atomic: the state check and the transition to `Runnable`
    // happen under the thread table lock. A second processor that wakes the
    // same thread sees `Runnable` and leaves the queue alone.
    if let Some(priority) = thread::claim_runnable(id) {
        let target = thread::cpu(id).unwrap_or(here);
        let target = if percpu::get(target)
            .is_some_and(|processor| processor.online.load(Ordering::Acquire))
        {
            target
        } else {
            here
        };
        if push_to(target, id, priority) {
            thread::set_cpu(id, target);
            woken = Some(priority);
            if target != here {
                remote = Some(target);
            }
        } else {
            thread::set_state(id, ThreadState::Blocked);
        }
    }
    if let Some(target) = remote {
        reschedule(target);
    }
    if remote.is_none() && woken.is_some_and(preempts_current) {
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

/// Timer hook: count the tick and preempt the running thread when its slice
/// has expired.
///
/// Runs inside the local APIC timer handler, after the end of interrupt, with
/// interrupts disabled. The user subsystem keeps its timer driven wait
/// queue on the boot processor; only that processor scans it.
pub fn on_tick() {
    let processor = percpu::this();
    percpu::record_heartbeat();
    // SAFETY: the timer handler runs with interrupts disabled on this
    // processor, so this is the only access to its slice counter.
    let expired = unsafe { processor.slice_mut().tick() };
    if processor.index == 0 {
        crate::user::on_tick();
    }
    if expired {
        schedule();
    }
}

/// Inter processor interrupt: run the scheduler on this processor.
///
/// Reached from the reschedule vector with interrupts disabled, after the end
/// of interrupt. A thread was placed in this processor's queue by another one.
pub fn on_ipi() {
    schedule();
}

/// Pick the next thread and switch to it.
///
/// Must run with interrupts disabled. If no thread can displace the running
/// one, it keeps the processor; a finished thread with nobody left to run stops
/// the processor instead of spinning in a stale context.
pub fn schedule() {
    let Some(current) = current_id() else {
        return;
    };
    let processor = percpu::this();
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
        let _guard = processor.lock_queue();
        // SAFETY: the guard owns this processor's queue lock, and no borrow
        // survives a switch.
        let queue: &mut RunQueue = unsafe { processor.queue_mut() };
        match queue.pick_next(current_slot) {
            Some(next) if next != current => {
                // The thread that leaves the processor goes to the back of its
                // priority group unless it cannot run again.
                if let Some((_, priority)) = current_slot {
                    let _ = queue.push(current, priority);
                }
                queue.remove(next);
                Some(next)
            }
            _ => None,
        }
    };

    if let Some(next) = next {
        if current_slot.is_some() {
            thread::set_state(current, ThreadState::Runnable);
        }
        thread::set_state(next, ThreadState::Running);
        thread::set_on_cpu(next, Some(processor.index));
        processor.current.store(next, Ordering::Release);
        // SAFETY: interrupts are disabled and the counter is not borrowed
        // anywhere, so this is the only access.
        unsafe { processor.slice_mut().reset() };

        let (Some(old_context), Some(new_context)) = (
            thread::context_mut_ptr(current),
            thread::context_mut_ptr(next),
        ) else {
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
        // The user stack slot is per processor, but a thread that blocked
        // inside a system call must return to its own stack; give it its saved
        // value.
        crate::user::install_user_stack(thread::user_stack(next));

        // Both `GS` bases name the block of the processor the thread runs on.
        // A migrating thread must not resume with the block of its previous
        // processor, so the values are rewritten before every switch.
        // SAFETY: the context belongs to `next`, which is not running on any
        // other processor, and interrupts are disabled here.
        unsafe {
            (*new_context).gs_base = processor.base();
            (*new_context).kernel_gs_base = processor.base();
        }

        // SAFETY: both pointers refer to contexts in the static thread table,
        // interrupts are disabled, and the switch restores the callee saved
        // registers it overwrites. The resume address is written first because
        // the assembly does not save RIP: `madrust_context_resume` pops the
        // return address of this call and continues in this function.
        unsafe {
            (*old_context).rip = madrust_context_resume as *const () as usize as u64;
            processor.record_departure(current);
            madrust_context_switch(old_context, new_context);
        }

        // Control returns here only when another thread switches back to this
        // one. Every thread that left this processor while it was away is no
        // longer on it, so its slot may be reclaimed. There can be more than
        // one, which is why the departures are kept in a ring.
        processor.take_departures(|out| thread::set_on_cpu(out, None));
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
        "MadRust: scheduler FAILED: context missing for thread {} -> {} (current cpu {:?} on_cpu {:?} queue {:?}; next cpu {:?} on_cpu {:?} queue {:?})",
        current,
        next,
        thread::cpu(current),
        thread::on_cpu(current),
        queue_of(current),
        thread::cpu(next),
        thread::on_cpu(next),
        queue_of(next)
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
/// Marks the thread finished, counts it on this processor and schedules; the
/// finished thread is never selected again, so this never returns.
#[no_mangle]
pub extern "C" fn madrust_thread_completed() -> ! {
    let _flags = sync::interrupt_save_disable();
    if let Some(current) = current_id() {
        thread::set_state(current, ThreadState::Finished);
        percpu::this().finished.fetch_add(1, Ordering::Release);
    }
    loop {
        schedule();
    }
}

/// Retire the calling thread after its exit system call.
pub fn retire_current() -> ! {
    let _flags = sync::interrupt_save_disable();
    if let Some(current) = current_id() {
        thread::set_state(current, ThreadState::Finished);
        percpu::this().finished.fetch_add(1, Ordering::Release);
    }
    loop {
        schedule();
    }
}
