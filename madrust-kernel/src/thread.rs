// SPDX-License-Identifier: GPL-2.0-only
//! Thread control blocks and kernel stacks (milestone M3).
//!
//! Every thread owns a page aligned 16 KiB stack from the kernel heap. The
//! thread table is a fixed size array so that spawning and retiring a thread
//! never allocates on the scheduling path; only the stack itself comes from
//! the heap.
//!
//! A thread is identified by its slot index. The identifier is stable for the
//! whole life of the thread and is what the run queue stores.

use core::cell::UnsafeCell;
use core::ptr::{addr_of, NonNull};

use madrust_sched::{Context, Priority, ThreadState, MAX_THREADS};

use crate::memory;

/// Size of every kernel thread stack.
pub const STACK_SIZE: usize = 16 * 1024;

/// Bytes below the top of a new stack reserved for the trampoline return
/// address. Keeping the initial RSP 16 byte aligned matches the SysV ABI once
/// the trampoline makes its first call.
const ENTRY_SLACK: usize = 16;

extern "C" {
    /// Address a brand new thread starts at.
    fn madrust_thread_trampoline();
    /// Address the trampoline returns to when the entry function returns.
    fn madrust_thread_completed() -> !;
}

/// Reason why a thread could not be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// Every slot of the thread table is in use.
    NoFreeSlot,
    /// The heap could not provide a stack.
    OutOfMemory,
}

/// Heap block that holds the stack of one thread.
struct Stack {
    base: NonNull<u8>,
    size: usize,
}

impl Stack {
    /// Take one page aligned stack from the kernel heap.
    fn allocate() -> Result<Self, SpawnError> {
        let base = memory::with(|manager| manager.alloc(STACK_SIZE))
            .ok_or(SpawnError::OutOfMemory)?
            .ok_or(SpawnError::OutOfMemory)?;
        Ok(Self {
            base,
            size: STACK_SIZE,
        })
    }

    /// Address one past the last usable byte.
    fn top(&self) -> u64 {
        self.base.as_ptr() as u64 + self.size as u64
    }
}

/// Entry function of one thread; the argument is the thread name.
pub(crate) type EntryFn = fn(&'static str);

/// First user context of a thread created by the `clone` system call.
///
/// The layout is the one `madrust_enter_user_thread` in
/// `madrust-kernel/src/user/syscall_entry.s` reads, byte for byte. It carries
/// the registers the child returns with: the parent's callee saved registers,
/// the return address of its `syscall`, the child stack pointer and RAX zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserStart {
    /// Instruction the child resumes at, the parent's `syscall` return address.
    pub rip: u64,
    /// Child stack pointer, the `clone` argument.
    pub rsp: u64,
    /// Child RFLAGS, with the interrupt flag forced on.
    pub rflags: u64,
    /// Child RAX: zero, the value `clone` returns in the child.
    pub rax: u64,
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
}

/// What a thread runs the first time the scheduler picks it.
pub(crate) enum Start {
    /// A kernel thread that calls an entry function.
    Kernel {
        function: EntryFn,
        name: &'static str,
    },
    /// A user thread that enters ring 3 with a saved register state.
    User(UserStart),
}

/// The three things the trampoline needs to start a thread.
pub(crate) struct StartEntry {
    /// Thread identifier, for the sanity check in `madrust_thread_start`.
    pub id: usize,
    /// What to run.
    pub start: Start,
}

/// One thread control block.
pub struct Thread {
    /// Stable identifier: the index of this slot.
    pub id: usize,
    /// Short name used by diagnostics and by the example entry function.
    pub name: &'static str,
    /// Current life cycle state.
    pub state: ThreadState,
    /// Priority the scheduler runs this thread at.
    pub priority: Priority,
    /// Saved machine state; filled on the first switch away.
    pub context: Context,
    /// Entry function, absent for the boot thread and for user threads.
    pub entry: Option<EntryFn>,
    /// First user context, absent for kernel threads.
    pub user: Option<UserStart>,
    /// Stack owned by this thread; absent for the boot thread.
    stack: Option<Stack>,
    /// Address one past the last byte of the kernel stack.
    ///
    /// The CPU installs this in `TSS.rsp0` when the thread runs, so interrupts
    /// and system calls from ring 3 arrive on the stack of the thread that
    /// caused them.
    kernel_stack_top: u64,
    /// Thread local storage base, loaded into `IA32_FS_BASE` on every switch.
    fs_base: u64,
    /// User address the kernel clears and wakes when the thread exits.
    clear_child_tid: u64,
    /// User stack pointer saved by the system call stub when the thread last
    /// entered the kernel.
    ///
    /// The stub reads `GS:0` on `sysretq`. That slot is per CPU, so a thread
    /// that blocks inside a system call must keep its own value in the TCB and
    /// have the scheduler put it back before the stub returns.
    user_stack: u64,
}

const EMPTY: Option<Thread> = None;

struct Table(UnsafeCell<[Option<Thread>; MAX_THREADS]>);

// SAFETY: the table is only reached through the helpers below. Every caller
// runs either during single-threaded boot or with interrupts disabled, and no
// borrow is held across a context switch.
unsafe impl Sync for Table {}

static TABLE: Table = Table(UnsafeCell::new([EMPTY; MAX_THREADS]));

/// Short-lived exclusive view of the table.
///
/// # Safety
///
/// The caller must guarantee that interrupts are disabled or that the kernel
/// is still single threaded, and must not keep the borrow across a context
/// switch.
unsafe fn table() -> &'static mut [Option<Thread>; MAX_THREADS] {
    // SAFETY: the caller upholds the exclusivity rule stated above.
    unsafe { &mut *TABLE.0.get() }
}

/// Register the currently running boot context as thread 0.
///
/// The boot thread owns no heap stack: it keeps running on the stack the
/// trampoline built, and its context is filled in on the first switch away.
pub(crate) fn install_boot() {
    // SAFETY: called once during boot with interrupts disabled, before any
    // other thread exists.
    let slots = unsafe { table() };
    slots[0] = Some(Thread {
        id: 0,
        name: "boot",
        state: ThreadState::Running,
        priority: Priority::DEFAULT,
        context: Context::ZERO,
        entry: None,
        user: None,
        stack: None,
        kernel_stack_top: 0,
        fs_base: 0,
        clear_child_tid: 0,
        user_stack: 0,
    });
}

/// Create a runnable thread that starts at `entry`.
///
/// The stack is prepared so that the trampoline can call `entry` and return to
/// [`madrust_thread_completed`] when it returns.
pub(crate) fn spawn(
    name: &'static str,
    entry: EntryFn,
    priority: Priority,
) -> Result<usize, SpawnError> {
    spawn_thread(name, Some(entry), None, 0, 0, 0, priority)
}

/// Create a runnable thread that enters ring 3 with `start`.
pub(crate) fn spawn_user(
    name: &'static str,
    start: UserStart,
    fs_base: u64,
    clear_child_tid: u64,
    kernel_gs_base: u64,
) -> Result<usize, SpawnError> {
    spawn_thread(
        name,
        None,
        Some(start),
        fs_base,
        clear_child_tid,
        kernel_gs_base,
        Priority::DEFAULT,
    )
}

/// Shared implementation of the two spawn paths.
fn spawn_thread(
    name: &'static str,
    entry: Option<EntryFn>,
    user: Option<UserStart>,
    fs_base: u64,
    clear_child_tid: u64,
    kernel_gs_base: u64,
    priority: Priority,
) -> Result<usize, SpawnError> {
    let id = {
        // SAFETY: the scheduler disables interrupts around every spawn.
        let slots = unsafe { table() };
        slots
            .iter()
            .position(Option::is_none)
            .ok_or(SpawnError::NoFreeSlot)?
    };

    let stack = Stack::allocate()?;
    let top = stack.top();

    // SAFETY: the stack block is at least STACK_SIZE bytes and 16 byte
    // aligned. `top - ENTRY_SLACK` leaves room for the return address that
    // `madrust_thread_trampoline` pops after the entry function returns.
    let entry_rsp = top - ENTRY_SLACK as u64;
    // SAFETY: `entry_rsp` lies inside the stack block just allocated.
    unsafe {
        (entry_rsp as *mut u64).write(madrust_thread_completed as *const () as usize as u64);
    }

    let context = Context {
        rsp: entry_rsp,
        rbx: 0,
        rbp: 0,
        // The trampoline passes the thread identifier to the Rust entry
        // point through R12.
        r12: id as u64,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: madrust_thread_trampoline as *const () as usize as u64,
        gs_base: 0,
        kernel_gs_base,
    };

    // SAFETY: the slot was free above and interrupts are disabled, so no other
    // thread can install or read the entry concurrently.
    let slots = unsafe { table() };
    slots[id] = Some(Thread {
        id,
        name,
        state: ThreadState::Runnable,
        priority,
        context,
        entry,
        user,
        stack: Some(stack),
        kernel_stack_top: top,
        fs_base,
        clear_child_tid,
        user_stack: 0,
    });
    Ok(id)
}

/// Current state of one thread.
pub(crate) fn state(id: usize) -> Option<ThreadState> {
    // SAFETY: the scheduler calls this with interrupts disabled and does not
    // hold the borrow afterwards.
    let slots = unsafe { table() };
    slots.get(id)?.as_ref().map(|thread| thread.state)
}

/// Priority of one thread.
pub(crate) fn priority(id: usize) -> Option<Priority> {
    // SAFETY: the scheduler calls this with interrupts disabled and does not
    // hold the borrow afterwards.
    let slots = unsafe { table() };
    slots.get(id)?.as_ref().map(|thread| thread.priority)
}

/// Write the state of one thread.
pub(crate) fn set_state(id: usize, state: ThreadState) {
    // SAFETY: the scheduler calls this with interrupts disabled and does not
    // hold the borrow afterwards.
    let slots = unsafe { table() };
    if let Some(thread) = slots.get_mut(id).and_then(Option::as_mut) {
        thread.state = state;
    }
}

/// Raw pointer to the saved context of one thread.
pub(crate) fn context_mut_ptr(id: usize) -> Option<*mut Context> {
    // SAFETY: the pointer is to static storage and remains valid while the
    // thread exists; no borrow is held on return.
    let slots = unsafe { table() };
    slots
        .get(id)?
        .as_ref()
        .map(|thread| addr_of!(thread.context) as *mut Context)
}

/// Raw pointer to the saved context of one thread.
pub(crate) fn context_ptr(id: usize) -> Option<*const Context> {
    // SAFETY: the pointer is to static storage and remains valid while the
    // thread exists; no borrow is held on return.
    let slots = unsafe { table() };
    slots
        .get(id)?
        .as_ref()
        .map(|thread| addr_of!(thread.context))
}

/// Identifier, start state and name of one thread.
pub(crate) fn entry(id: usize) -> Option<StartEntry> {
    // SAFETY: called from the new thread with interrupts disabled by the
    // switch, and the borrow ends before the entry function runs.
    let slots = unsafe { table() };
    let thread = slots.get(id)?.as_ref()?;
    let start = match (thread.entry, thread.user) {
        (Some(function), _) => Start::Kernel {
            function,
            name: thread.name,
        },
        (None, Some(user)) => Start::User(user),
        (None, None) => return None,
    };
    Some(StartEntry {
        id: thread.id,
        start,
    })
}

/// Address one past the last byte of the kernel stack of `id`.
pub(crate) fn kernel_stack_top(id: usize) -> Option<u64> {
    // SAFETY: the scheduler calls this with interrupts disabled and does not
    // keep the borrow.
    let slots = unsafe { table() };
    slots
        .get(id)?
        .as_ref()
        .map(|thread| thread.kernel_stack_top)
}

/// Thread local storage base of `id`.
pub(crate) fn fs_base(id: usize) -> Option<u64> {
    // SAFETY: the scheduler calls this with interrupts disabled and does not
    // keep the borrow.
    let slots = unsafe { table() };
    slots.get(id)?.as_ref().map(|thread| thread.fs_base)
}

/// Set the thread local storage base of `id`.
pub(crate) fn set_fs_base(id: usize, base: u64) {
    // SAFETY: the caller runs with interrupts disabled and does not keep the
    // borrow.
    let slots = unsafe { table() };
    if let Some(thread) = slots.get_mut(id).and_then(Option::as_mut) {
        thread.fs_base = base;
    }
}

/// User address the kernel must clear when `id` exits.
pub(crate) fn clear_child_tid(id: usize) -> u64 {
    // SAFETY: the caller runs with interrupts disabled and does not keep the
    // borrow.
    let slots = unsafe { table() };
    slots
        .get(id)
        .and_then(Option::as_ref)
        .map_or(0, |thread| thread.clear_child_tid)
}

/// User stack pointer saved by the system call stub for `id`.
pub(crate) fn user_stack(id: usize) -> u64 {
    // SAFETY: the caller runs with interrupts disabled and does not keep the
    // borrow.
    let slots = unsafe { table() };
    slots
        .get(id)
        .and_then(Option::as_ref)
        .map_or(0, |thread| thread.user_stack)
}

/// Replace the saved user stack pointer of `id`.
pub(crate) fn set_user_stack(id: usize, stack: u64) {
    // SAFETY: the caller runs with interrupts disabled and does not keep the
    // borrow.
    let slots = unsafe { table() };
    if let Some(thread) = slots.get_mut(id).and_then(Option::as_mut) {
        thread.user_stack = stack;
    }
}

/// Set the kernel stack top of the boot thread.
pub(crate) fn set_kernel_stack_top(id: usize, top: u64) {
    // SAFETY: called once during scheduler init with interrupts disabled.
    let slots = unsafe { table() };
    if let Some(thread) = slots.get_mut(id).and_then(Option::as_mut) {
        thread.kernel_stack_top = top;
    }
}

/// Remove a thread that never entered the run queue.
pub(crate) fn remove(id: usize) -> bool {
    // SAFETY: spawn holds the interrupt critical section and the thread was
    // not published when this is used, so nobody else can observe the slot.
    let slots = unsafe { table() };
    let removed = slots.get_mut(id).and_then(Option::take);
    match removed {
        Some(thread) => {
            release_stack(thread.stack);
            true
        }
        None => false,
    }
}

/// Free the stacks of every finished thread and forget the slots.
///
/// The boot thread calls this once every example thread has retired, so no
/// thread can be running on a stack that is handed back to the heap.
pub(crate) fn reclaim_finished() -> usize {
    let flags = crate::sync::interrupt_save_disable();
    // SAFETY: interrupts are disabled for the whole mutation, so the timer
    // handler cannot schedule a thread while the table is borrowed.
    let slots = unsafe { table() };
    let mut reclaimed = 0;
    for slot in slots.iter_mut() {
        if !slot
            .as_ref()
            .is_some_and(|thread| thread.state == ThreadState::Finished)
        {
            continue;
        }
        if let Some(thread) = slot.take() {
            release_stack(thread.stack);
            reclaimed += 1;
        }
    }
    crate::sync::interrupt_restore(flags);
    reclaimed
}

/// Give one stack back to the heap, if the thread owned one.
fn release_stack(stack: Option<Stack>) {
    if let Some(stack) = stack {
        let _ = memory::with(|manager| manager.dealloc(stack.base, stack.size));
    }
}
