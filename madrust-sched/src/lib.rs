// SPDX-License-Identifier: GPL-2.0-only
//! Thread state, saved context and run queue for MadRust (milestone E3a).
//!
//! This crate holds the parts of the scheduler that do not touch hardware, so
//! the ordering rules run as host unit tests. The kernel keeps the assembly
//! context switch, the timer hook and the thread control blocks that own a
//! stack in `madrust-kernel`.
//!
//! ## Policy
//!
//! Every thread carries a [`Priority`]. The scheduler runs the highest
//! priority that is runnable; a lower priority never displaces a higher one.
//! The run queue is a fixed size array of thread identifiers ordered by
//! arrival. It holds the threads that may run, and normally never the thread
//! that is currently running.
//!
//! Two threads of the same priority run round robin. A switch removes the
//! next thread from the queue and pushes the thread it left behind to the
//! back, so the queue position inside one priority is the round-robin order.
//!
//! A [`Slice`] counts the timer ticks a thread may keep the CPU before the
//! timer preempts it. The slice is reset when a thread is scheduled in.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

/// Maximum number of threads the kernel can hold at once.
///
/// The kernel reserves one idle slot per processor, so the bound leaves room
/// for the boot thread, the idle threads of every processor and the workloads.
pub const MAX_THREADS: usize = 32;

/// Scheduling priority of one thread.
///
/// A larger value runs first. The range is deliberately small: the policy is a
/// strict priority with round robin inside one level, not a weight. The kernel
/// gives every thread [`Priority::DEFAULT`] unless it asks for another value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(u8);

impl Priority {
    /// Lowest priority allowed.
    pub const LOWEST: Self = Self(0);
    /// Priority every thread starts with.
    pub const DEFAULT: Self = Self(8);
    /// Highest priority allowed.
    pub const HIGHEST: Self = Self(15);

    /// Clamp `value` into the supported range.
    pub const fn new(value: u8) -> Self {
        if value > Self::HIGHEST.0 {
            Self::HIGHEST
        } else {
            Self(value)
        }
    }

    /// Numeric value of this priority.
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Timer ticks a thread may run before the scheduler preempts it.
///
/// The value is a count of ticks, not milliseconds: the kernel turns its timer
/// period into a slice by choosing a number here. A zero is treated as one so a
/// misconfigured slice cannot spin the timer without progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slice {
    ticks: u64,
    left: u64,
}

impl Slice {
    /// A slice of `ticks` timer ticks.
    pub const fn new(ticks: u64) -> Self {
        let ticks = if ticks == 0 { 1 } else { ticks };
        Self { ticks, left: ticks }
    }

    /// Configured length in ticks.
    pub const fn ticks(self) -> u64 {
        self.ticks
    }

    /// Ticks left before the slice expires.
    pub const fn remaining(self) -> u64 {
        self.left
    }

    /// Give the thread a fresh slice after it was scheduled in.
    pub fn reset(&mut self) {
        self.left = self.ticks;
    }

    /// Count one timer tick.
    ///
    /// Returns `true` when the slice has just expired; the counter restarts in
    /// that case, so the caller can preempt and keep counting for the thread it
    /// switches to.
    pub fn tick(&mut self) -> bool {
        if self.left > 0 {
            self.left -= 1;
        }
        if self.left == 0 {
            self.left = self.ticks;
            true
        } else {
            false
        }
    }
}

/// Saved machine state of one thread.
///
/// The layout is the one `madrust_context_switch` in
/// `madrust-kernel/src/context.s` reads and writes, byte for byte. The kernel
/// never saves RFLAGS here: interrupts stay disabled while the switch runs,
/// and each thread restores its own flag state through its interrupt frame or
/// through the trampoline.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// Stack pointer at the moment of the switch.
    pub rsp: u64,
    /// Callee saved registers.
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    /// Address the switch jumps to when the thread runs again.
    pub rip: u64,
    /// `IA32_GS_BASE` at the moment of the switch.
    ///
    /// The system call stub uses `swapgs`, so the active GS base depends on
    /// whether the thread was interrupted in ring 3 or inside a system call.
    /// Saving both MSRs keeps that state with the thread instead of the CPU.
    pub gs_base: u64,
    /// `IA32_KERNEL_GS_BASE` at the moment of the switch.
    pub kernel_gs_base: u64,
}

impl Context {
    /// Context of a thread that has never run.
    pub const ZERO: Self = Self {
        rsp: 0,
        rbx: 0,
        rbp: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: 0,
        gs_base: 0,
        kernel_gs_base: 0,
    };
}

/// Life cycle state of one thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    /// May be selected by the scheduler.
    Runnable,
    /// Currently owning the CPU.
    Running,
    /// Waiting for an event; never selected.
    Blocked,
    /// Entry function returned; never selected again.
    Finished,
}

impl ThreadState {
    /// `true` when the state can be placed in the run queue.
    pub const fn is_runnable(self) -> bool {
        matches!(self, Self::Runnable | Self::Running)
    }

    /// `true` when moving from `self` to `next` is legal.
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Runnable,
                Self::Running | Self::Blocked | Self::Finished
            ) | (
                Self::Running,
                Self::Runnable | Self::Blocked | Self::Finished
            ) | (Self::Blocked, Self::Runnable | Self::Finished)
        )
    }

    /// Validate a state change.
    pub fn transition(self, next: Self) -> Result<Self, StateError> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(StateError {
                from: self,
                to: next,
            })
        }
    }
}

/// Rejected state change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateError {
    /// State the thread was in.
    pub from: ThreadState,
    /// State the caller asked for.
    pub to: ThreadState,
}

/// Reason why an identifier could not enter the run queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The queue already holds [`MAX_THREADS`] identifiers.
    Full,
    /// The identifier is already queued.
    AlreadyQueued,
}

/// One queued thread with the priority it runs at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    id: usize,
    priority: Priority,
}

const EMPTY_ENTRY: Entry = Entry {
    id: 0,
    priority: Priority::LOWEST,
};

/// Fixed size priority run queue.
///
/// An array avoids an allocation on the scheduling path, which matters because
/// the timer handler may schedule while the heap lock is unavailable. The
/// identifiers are indices into the kernel thread table; the queue owns no
/// thread state. Entries keep their insertion order, which is the round-robin
/// order inside one priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunQueue {
    entries: [Entry; MAX_THREADS],
    len: usize,
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl RunQueue {
    /// Empty queue.
    pub const fn new() -> Self {
        Self {
            entries: [EMPTY_ENTRY; MAX_THREADS],
            len: 0,
        }
    }

    /// Number of queued identifiers.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// `true` when no thread is queued.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `true` when `id` is queued.
    pub fn contains(&self, id: usize) -> bool {
        self.entries[..self.len].iter().any(|entry| entry.id == id)
    }

    /// Append `id` at `priority` to the back of its priority group.
    pub fn push(&mut self, id: usize, priority: Priority) -> Result<(), QueueError> {
        if self.contains(id) {
            return Err(QueueError::AlreadyQueued);
        }
        if self.len == MAX_THREADS {
            return Err(QueueError::Full);
        }
        self.entries[self.len] = Entry { id, priority };
        self.len += 1;
        Ok(())
    }

    /// Remove `id`, keeping the order of the remaining identifiers.
    ///
    /// Returns `false` when the identifier was not queued.
    pub fn remove(&mut self, id: usize) -> bool {
        let Some(position) = self.entries[..self.len]
            .iter()
            .position(|entry| entry.id == id)
        else {
            return false;
        };
        self.entries.copy_within(position + 1..self.len, position);
        self.len -= 1;
        true
    }

    /// Highest priority queued identifier that is not `current`.
    ///
    /// `current` carries the identifier and priority of the running thread, or
    /// `None` when the caller is not runnable and may hand the CPU to any
    /// priority. A runnable current thread is only displaced by an entry of at
    /// least its own priority; round robin inside that priority then picks the
    /// entry that has waited longest.
    pub fn pick_next(&self, current: Option<(usize, Priority)>) -> Option<usize> {
        let current_id = current.map(|(id, _)| id);
        let mut best: Option<Entry> = None;
        for entry in &self.entries[..self.len] {
            if Some(entry.id) == current_id {
                continue;
            }
            let replaces = best.is_none_or(|queued| entry.priority > queued.priority);
            if replaces {
                best = Some(*entry);
            }
        }
        let best = best?;
        if current.is_some_and(|(_, priority)| best.priority < priority) {
            return None;
        }
        Some(best.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_layout_matches_the_assembly() {
        use core::mem::{offset_of, size_of};

        assert_eq!(offset_of!(Context, rsp), 0x00);
        assert_eq!(offset_of!(Context, rbx), 0x08);
        assert_eq!(offset_of!(Context, rbp), 0x10);
        assert_eq!(offset_of!(Context, r12), 0x18);
        assert_eq!(offset_of!(Context, r13), 0x20);
        assert_eq!(offset_of!(Context, r14), 0x28);
        assert_eq!(offset_of!(Context, r15), 0x30);
        assert_eq!(offset_of!(Context, rip), 0x38);
        assert_eq!(offset_of!(Context, gs_base), 0x40);
        assert_eq!(offset_of!(Context, kernel_gs_base), 0x48);
        assert_eq!(size_of::<Context>(), 0x50);
    }

    #[test]
    fn queue_rejects_duplicates_and_overflow() {
        let mut queue = RunQueue::new();
        assert_eq!(queue.push(1, Priority::DEFAULT), Ok(()));
        assert_eq!(
            queue.push(1, Priority::DEFAULT),
            Err(QueueError::AlreadyQueued)
        );
        for id in 2..MAX_THREADS {
            assert_eq!(queue.push(id, Priority::DEFAULT), Ok(()));
        }
        assert_eq!(queue.len(), MAX_THREADS - 1);
        assert_eq!(queue.push(0, Priority::DEFAULT), Ok(()));
        assert_eq!(queue.len(), MAX_THREADS);
        assert_eq!(
            queue.push(MAX_THREADS, Priority::DEFAULT),
            Err(QueueError::Full)
        );
    }

    #[test]
    fn queue_removes_in_order() {
        let mut queue = RunQueue::new();
        for id in 0..4 {
            assert_eq!(queue.push(id, Priority::DEFAULT), Ok(()));
        }
        assert!(queue.remove(1));
        assert_eq!(queue.len(), 3);
        assert!(!queue.remove(1));
        assert_eq!(queue.pick_next(None), Some(0));
        assert_eq!(queue.pick_next(Some((0, Priority::DEFAULT))), Some(2));
    }

    #[test]
    fn picks_round_robin_and_skips_the_running_thread() {
        // Ready queue never holds the running thread: start with 1 and 2,
        // boot thread 0 running.
        let mut queue = RunQueue::new();
        queue.push(1, Priority::DEFAULT).unwrap();
        queue.push(2, Priority::DEFAULT).unwrap();

        let mut current = 0;
        let mut order = [0usize; 6];
        for slot in &mut order {
            let next = queue.pick_next(Some((current, Priority::DEFAULT))).unwrap();
            assert_ne!(next, current);
            queue.remove(next);
            queue.push(current, Priority::DEFAULT).unwrap();
            *slot = next;
            current = next;
        }

        assert_eq!(order, [1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn skips_the_running_thread_wherever_it_is_queued() {
        let mut queue = RunQueue::new();
        for id in [3, 1, 2] {
            queue.push(id, Priority::DEFAULT).unwrap();
        }
        assert_eq!(queue.pick_next(Some((1, Priority::DEFAULT))), Some(3));
        assert_eq!(queue.pick_next(Some((3, Priority::DEFAULT))), Some(1));
        assert_eq!(queue.pick_next(None), Some(3));
    }

    #[test]
    fn higher_priority_wins_regardless_of_arrival_order() {
        let mut queue = RunQueue::new();
        let high = Priority::new(12);
        let low = Priority::new(2);
        queue.push(1, low).unwrap();
        queue.push(2, high).unwrap();
        queue.push(3, Priority::DEFAULT).unwrap();
        assert_eq!(queue.pick_next(None), Some(2));

        // The same priority class keeps its insertion order.
        assert!(queue.remove(2));
        assert_eq!(queue.pick_next(None), Some(3));
        assert!(queue.remove(3));
        assert_eq!(queue.pick_next(None), Some(1));
    }

    #[test]
    fn a_lower_priority_does_not_displace_a_running_thread() {
        let mut queue = RunQueue::new();
        queue.push(1, Priority::new(2)).unwrap();
        assert_eq!(
            queue.pick_next(Some((0, Priority::DEFAULT))),
            None,
            "a runnable higher priority thread keeps the CPU"
        );
        // A blocked or finished current thread lets the lower priority run.
        assert_eq!(queue.pick_next(None), Some(1));
        // An equal priority thread does displace it, which is the round robin.
        assert_eq!(
            queue.pick_next(Some((0, Priority::new(2)))),
            Some(1),
            "same priority round robins"
        );
    }

    #[test]
    fn equal_priorities_take_turns() {
        let mut queue = RunQueue::new();
        let priority = Priority::DEFAULT;
        queue.push(1, priority).unwrap();
        queue.push(2, priority).unwrap();
        queue.push(3, priority).unwrap();

        let mut current = 0;
        let mut order = [0usize; 6];
        for slot in &mut order {
            let next = queue.pick_next(Some((current, priority))).unwrap();
            queue.remove(next);
            queue.push(current, priority).unwrap();
            *slot = next;
            current = next;
        }
        assert_eq!(order, [1, 2, 3, 0, 1, 2]);
    }

    #[test]
    fn empty_queue_has_no_successor() {
        let queue = RunQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.pick_next(None), None);
        assert_eq!(queue.pick_next(Some((0, Priority::DEFAULT))), None);
    }

    #[test]
    fn slice_expires_after_its_ticks_and_restarts() {
        let mut slice = Slice::new(2);
        assert_eq!(slice.ticks(), 2);
        assert_eq!(slice.remaining(), 2);
        assert!(!slice.tick());
        assert_eq!(slice.remaining(), 1);
        assert!(slice.tick(), "the second tick ends a two tick slice");
        assert_eq!(slice.remaining(), 2);
        assert!(!slice.tick());
        slice.reset();
        assert_eq!(slice.remaining(), 2);
    }

    #[test]
    fn a_zero_slice_still_counts_one_tick() {
        let mut slice = Slice::new(0);
        assert_eq!(slice.ticks(), 1);
        assert!(slice.tick());
    }

    #[test]
    fn priorities_clamp_to_the_supported_range() {
        assert_eq!(Priority::new(0), Priority::LOWEST);
        assert_eq!(Priority::new(200), Priority::HIGHEST);
        assert_eq!(Priority::default(), Priority::DEFAULT);
        assert!(Priority::HIGHEST > Priority::DEFAULT);
        assert_eq!(Priority::DEFAULT.value(), 8);
    }

    #[test]
    fn states_follow_the_life_cycle() {
        use ThreadState::{Blocked, Finished, Runnable, Running};

        assert_eq!(Runnable.transition(Running), Ok(Running));
        assert_eq!(Running.transition(Runnable), Ok(Runnable));
        assert_eq!(Running.transition(Blocked), Ok(Blocked));
        assert_eq!(Blocked.transition(Runnable), Ok(Runnable));
        assert_eq!(Running.transition(Finished), Ok(Finished));

        assert_eq!(
            Finished.transition(Runnable),
            Err(StateError {
                from: Finished,
                to: Runnable,
            })
        );
        assert_eq!(
            Blocked.transition(Running),
            Err(StateError {
                from: Blocked,
                to: Running,
            })
        );
    }

    #[test]
    fn only_runnable_and_running_enter_the_queue() {
        assert!(ThreadState::Runnable.is_runnable());
        assert!(ThreadState::Running.is_runnable());
        assert!(!ThreadState::Blocked.is_runnable());
        assert!(!ThreadState::Finished.is_runnable());
    }
}
