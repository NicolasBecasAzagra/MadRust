// SPDX-License-Identifier: GPL-2.0-only
//! Thread state, saved context and run queue for MadRust (milestone M3).
//!
//! This crate holds the parts of the scheduler that do not touch hardware, so
//! the ordering rules run as host unit tests. The kernel keeps the assembly
//! context switch, the timer hook and the thread control blocks that own a
//! stack in `madrust-kernel`.
//!
//! The run queue is a fixed size array of thread identifiers. It holds the
//! threads that may run, never the thread that is currently running: the
//! scheduler removes the next thread from the queue and pushes the thread it
//! left behind to the back, which is what makes the policy round robin.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

/// Maximum number of threads the kernel can hold at once.
pub const MAX_THREADS: usize = 8;

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

/// Fixed size round-robin run queue.
///
/// An array avoids an allocation on the scheduling path, which matters because
/// the timer handler may schedule while the heap lock is unavailable. The
/// identifiers are indices into the kernel thread table; the queue owns no
/// thread state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunQueue {
    ids: [usize; MAX_THREADS],
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
            ids: [0; MAX_THREADS],
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
        self.ids[..self.len].contains(&id)
    }

    /// Append `id` to the back of the queue.
    pub fn push(&mut self, id: usize) -> Result<(), QueueError> {
        if self.contains(id) {
            return Err(QueueError::AlreadyQueued);
        }
        if self.len == MAX_THREADS {
            return Err(QueueError::Full);
        }
        self.ids[self.len] = id;
        self.len += 1;
        Ok(())
    }

    /// Remove `id`, keeping the order of the remaining identifiers.
    ///
    /// Returns `false` when the identifier was not queued.
    pub fn remove(&mut self, id: usize) -> bool {
        let Some(position) = self.ids[..self.len].iter().position(|&queued| queued == id) else {
            return false;
        };
        self.ids.copy_within(position + 1..self.len, position);
        self.len -= 1;
        true
    }

    /// First queued identifier that is not `current`.
    ///
    /// This is the whole selection policy: the caller removes the returned
    /// thread and pushes the thread it left behind to the back.
    pub fn pick_next(&self, current: Option<usize>) -> Option<usize> {
        self.ids[..self.len]
            .iter()
            .copied()
            .find(|&id| Some(id) != current)
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
        assert_eq!(size_of::<Context>(), 0x40);
    }

    #[test]
    fn queue_rejects_duplicates_and_overflow() {
        let mut queue = RunQueue::new();
        assert_eq!(queue.push(1), Ok(()));
        assert_eq!(queue.push(1), Err(QueueError::AlreadyQueued));
        for id in 2..MAX_THREADS {
            assert_eq!(queue.push(id), Ok(()));
        }
        assert_eq!(queue.len(), MAX_THREADS - 1);
        assert_eq!(queue.push(0), Ok(()));
        assert_eq!(queue.len(), MAX_THREADS);
        assert_eq!(queue.push(MAX_THREADS), Err(QueueError::Full));
    }

    #[test]
    fn queue_removes_in_order() {
        let mut queue = RunQueue::new();
        for id in 0..4 {
            assert_eq!(queue.push(id), Ok(()));
        }
        assert!(queue.remove(1));
        assert_eq!(queue.len(), 3);
        assert!(!queue.remove(1));
        assert_eq!(queue.pick_next(None), Some(0));
        assert_eq!(queue.pick_next(Some(0)), Some(2));
    }

    #[test]
    fn picks_round_robin_and_skips_the_running_thread() {
        // Ready queue never holds the running thread: start with 1 and 2,
        // boot thread 0 running.
        let mut queue = RunQueue::new();
        queue.push(1).unwrap();
        queue.push(2).unwrap();

        let mut current = 0;
        let mut order = [0usize; 6];
        for slot in &mut order {
            let next = queue.pick_next(Some(current)).unwrap();
            assert_ne!(next, current);
            queue.remove(next);
            queue.push(current).unwrap();
            *slot = next;
            current = next;
        }

        assert_eq!(order, [1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn skips_the_running_thread_wherever_it_is_queued() {
        let mut queue = RunQueue::new();
        for id in [3, 1, 2] {
            queue.push(id).unwrap();
        }
        assert_eq!(queue.pick_next(Some(1)), Some(3));
        assert_eq!(queue.pick_next(Some(3)), Some(1));
        assert_eq!(queue.pick_next(None), Some(3));
    }

    #[test]
    fn empty_queue_has_no_successor() {
        let queue = RunQueue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.pick_next(None), None);
        assert_eq!(queue.pick_next(Some(0)), None);
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
