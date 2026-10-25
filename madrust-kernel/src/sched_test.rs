// SPDX-License-Identifier: GPL-2.0-only
//! Scheduler self test: wakeup latency and slice tail (milestone E3a).
//!
//! The test runs on the target, the way the allocator self test does, and
//! reports the numbers in the boot log. Both measurements use the calibrated
//! time stamp counter, so they are not quantised by the 10 ms timer tick.
//!
//! ## Wakeup latency
//!
//! A waiter blocks on a futex word. A waker waits until the waiter is really
//! blocked, stores the next value, takes the time stamp and calls
//! `FUTEX_WAKE`. The wakeup path preempts the waker, so the waiter resumes
//! inside the wake and records the time since that stamp. One iteration per
//! block/wake pair, so the two threads ping pong.
//!
//! ## Slice tail
//!
//! A runner takes the time stamp and burns the rest of its slice in a loop.
//! An observer of the same priority is already runnable, so it runs when the
//! runner's slice expires. The difference is the tail a runnable thread sees
//! when it has to wait for the slice of the thread in front of it.
//!
//! Both pairs run at [`Priority::HIGHEST`] so the boot thread parks out of the
//! rotation: the numbers then describe the policy, not the test harness.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use madrust_arch::cpu;
use madrust_sched::{Priority, ThreadState};

use crate::clock;
use crate::pit;
use crate::sched;
use crate::sync;
use crate::thread;
use crate::user::futex;

/// Ping-pong iterations of the wakeup measurement.
const WAKE_ITERATIONS: usize = 32;

/// Wakeups that warm the path before the measurement starts.
///
/// The first pass through the wake path is cold: its code and data are not in
/// the caches yet. Two warmups keep that one time cost out of the tail.
const WAKE_WARMUP: usize = 2;

/// Total warmup and measured wakeup pairs.
const WAKE_TOTAL: usize = WAKE_WARMUP + WAKE_ITERATIONS;

/// Slice waits measured for the tail.
const SLICE_ITERATIONS: usize = 8;

/// Upper bound on one test, in timer ticks (two seconds).
const TEST_TIMEOUT_TICKS: u64 = 2 * pit::TICK_HZ;

/// A tenth of one timer tick: the wakeup latency must stay below this.
const WAKE_LIMIT_US: u64 = 1_000_000 / pit::TICK_HZ / 10;

static WAITER_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static WAKE_WORD: AtomicU32 = AtomicU32::new(0);
static WAKE_START: [AtomicU64; WAKE_TOTAL] = [const { AtomicU64::new(0) }; WAKE_TOTAL];
static WAKE_LATENCY: [AtomicU64; WAKE_ITERATIONS] = [const { AtomicU64::new(0) }; WAKE_ITERATIONS];
static WAKE_STAMP: AtomicU64 = AtomicU64::new(0);
static WAKE_DONE: AtomicBool = AtomicBool::new(false);
static WAKE_FAILED: AtomicBool = AtomicBool::new(false);

static SLICE_START: [AtomicU64; SLICE_ITERATIONS] = [const { AtomicU64::new(0) }; SLICE_ITERATIONS];
static SLICE_LATENCY: [AtomicU64; SLICE_ITERATIONS] =
    [const { AtomicU64::new(0) }; SLICE_ITERATIONS];
static SLICE_SEEN: [AtomicBool; SLICE_ITERATIONS] =
    [const { AtomicBool::new(false) }; SLICE_ITERATIONS];
static SLICE_DONE: AtomicBool = AtomicBool::new(false);
static SLICE_FAILED: AtomicBool = AtomicBool::new(false);

/// Run both measurements. Returns `false` when an assertion fails.
pub fn run() -> bool {
    wakeup_test() && slice_test()
}

/// Measure the delay between a futex wake and the woken thread running.
fn wakeup_test() -> bool {
    let finished_before = sched::finished_threads();
    let Ok(waiter) = sched::spawn_with_priority("W", wake_waiter, Priority::HIGHEST) else {
        crate::kprintln!("SCHED wakeup FAILED: cannot create the waiter");
        return false;
    };
    WAITER_ID.store(waiter, Ordering::Release);
    if sched::spawn_with_priority("K", wake_waker, Priority::HIGHEST).is_err() {
        crate::kprintln!("SCHED wakeup FAILED: cannot create the waker");
        return false;
    }

    let finished = wait_for_threads(finished_before, 2);
    if !finished || WAKE_FAILED.load(Ordering::Acquire) {
        crate::kprintln!("SCHED wakeup FAILED: the test did not complete");
        return false;
    }
    WAKE_DONE.load(Ordering::Acquire)
}

/// Measure how long a runnable thread waits for the slice in front of it.
fn slice_test() -> bool {
    let finished_before = sched::finished_threads();
    if sched::spawn_with_priority("R", slice_runner, Priority::HIGHEST).is_err() {
        crate::kprintln!("SCHED slice FAILED: cannot create the runner");
        return false;
    }
    if sched::spawn_with_priority("O", slice_observer, Priority::HIGHEST).is_err() {
        crate::kprintln!("SCHED slice FAILED: cannot create the observer");
        return false;
    }

    let finished = wait_for_threads(finished_before, 2);
    if !finished || SLICE_FAILED.load(Ordering::Acquire) {
        crate::kprintln!("SCHED slice FAILED: the test did not complete");
        return false;
    }
    SLICE_DONE.load(Ordering::Acquire)
}

/// Entry function of the thread that waits on the futex.
fn wake_waiter(_name: &'static str) {
    let address = &WAKE_WORD as *const AtomicU32 as u64;
    for iteration in 0..WAKE_TOTAL {
        // The waker stores `iteration + 1` before it wakes us, so the word
        // already holds the value of this iteration when the loop starts.
        let value = WAKE_WORD.load(Ordering::Acquire);
        let expected = iteration as u32;
        if iteration < WAKE_WARMUP {
            if futex::wait(address, value, expected, None) != 0 {
                WAKE_FAILED.store(true, Ordering::Release);
                return;
            }
            continue;
        }
        let result = futex::wait_stamped(address, value, expected, None, &WAKE_STAMP);
        let now = WAKE_STAMP.load(Ordering::Acquire);
        if result != 0 || value != expected || now == 0 {
            WAKE_FAILED.store(true, Ordering::Release);
            return;
        }
        let start = WAKE_START[iteration].load(Ordering::Acquire);
        WAKE_LATENCY[iteration - WAKE_WARMUP].store(
            clock::microseconds(now.wrapping_sub(start)),
            Ordering::Release,
        );
    }
}

/// Entry function of the thread that produces the wakeups.
fn wake_waker(_name: &'static str) {
    let address = &WAKE_WORD as *const AtomicU32 as u64;
    let waiter = WAITER_ID.load(Ordering::Acquire);
    let deadline = pit::ticks() + TEST_TIMEOUT_TICKS;

    for (iteration, start) in WAKE_START.iter().enumerate() {
        // A wakeup that lands before the waiter is registered would be lost,
        // so wait for the thread state the wait path sets last.
        while !thread_blocked(waiter) {
            if pit::ticks() >= deadline {
                WAKE_FAILED.store(true, Ordering::Release);
                return;
            }
            sched::yield_now();
        }

        // The stamp and the wake share one interrupt free section, so a
        // timer tick cannot land between them and charge the wake with the
        // rest of a slice.
        let flags = sync::interrupt_save_disable();
        WAKE_WORD.store(iteration as u32 + 1, Ordering::Release);
        start.store(cpu::read_tsc(), Ordering::Release);
        let woken = futex::wake(address, 1);
        sync::interrupt_restore(flags);
        if woken != 1 {
            WAKE_FAILED.store(true, Ordering::Release);
            return;
        }
    }

    report("wakeup", &WAKE_LATENCY, true);
    WAKE_DONE.store(true, Ordering::Release);
}

/// Entry function of the thread that burns a slice while the observer waits.
fn slice_runner(_name: &'static str) {
    let deadline = pit::ticks() + TEST_TIMEOUT_TICKS;
    for (start, seen) in SLICE_START.iter().zip(SLICE_SEEN.iter()) {
        start.store(cpu::read_tsc(), Ordering::Release);
        while !seen.load(Ordering::Acquire) {
            if pit::ticks() >= deadline {
                SLICE_FAILED.store(true, Ordering::Release);
                return;
            }
            core::hint::spin_loop();
        }
    }
}

/// Entry function of the already runnable thread that records its wait.
fn slice_observer(_name: &'static str) {
    let deadline = pit::ticks() + TEST_TIMEOUT_TICKS;
    for (start_slot, (latency_slot, seen)) in SLICE_START
        .iter()
        .zip(SLICE_LATENCY.iter().zip(SLICE_SEEN.iter()))
    {
        let start = loop {
            let start = start_slot.load(Ordering::Acquire);
            if start != 0 {
                break start;
            }
            if pit::ticks() >= deadline {
                SLICE_FAILED.store(true, Ordering::Release);
                SLICE_DONE.store(true, Ordering::Release);
                return;
            }
            sched::yield_now();
        };
        let now = cpu::read_tsc();
        latency_slot.store(
            clock::microseconds(now.wrapping_sub(start)),
            Ordering::Release,
        );
        seen.store(true, Ordering::Release);
        // Go back behind the runner: it then takes the CPU for its next slice.
        sched::yield_now();
    }

    report("slice", &SLICE_LATENCY, false);
    SLICE_DONE.store(true, Ordering::Release);
}

/// Print the minimum, median and maximum and check the wakeup limit.
///
/// `enforce` turns the limit into an assertion; the slice tail has no fixed
/// bound because it is bounded by the slice itself.
fn report(name: &str, samples: &[AtomicU64], enforce: bool) {
    let mut values = [0u64; WAKE_ITERATIONS];
    let count = samples.len();
    for (slot, sample) in values.iter_mut().zip(samples.iter()) {
        *slot = sample.load(Ordering::Acquire);
    }
    sort(&mut values[..count]);
    let min = values[0];
    let median = if count.is_multiple_of(2) {
        (values[count / 2 - 1] + values[count / 2]) / 2
    } else {
        values[count / 2]
    };
    let max = values[count - 1];
    crate::kprintln!(
        "SCHED {} us min={} median={} max={}",
        name,
        min,
        median,
        max
    );
    if enforce && max >= WAKE_LIMIT_US {
        crate::kprintln!(
            "SCHED {} FAILED: max {} us is not below {} us",
            name,
            max,
            WAKE_LIMIT_US
        );
        WAKE_FAILED.store(true, Ordering::Release);
    }
}

/// Insertion sort of a small sample array, so the median needs no allocator.
fn sort(values: &mut [u64]) {
    for index in 1..values.len() {
        let mut position = index;
        while position > 0 && values[position - 1] > values[position] {
            values.swap(position - 1, position);
            position -= 1;
        }
    }
}

/// `true` while `id` is blocked in the futex wait path.
fn thread_blocked(id: usize) -> bool {
    let flags = sync::interrupt_save_disable();
    let blocked = thread::state(id) == Some(ThreadState::Blocked);
    sync::interrupt_restore(flags);
    blocked
}

/// Wait until `count` more threads retired, without spinning on the CPU.
///
/// The boot thread runs at the default priority, so the timer parks it here
/// while the high priority test threads run.
fn wait_for_threads(finished_before: usize, count: usize) -> bool {
    let deadline = pit::ticks() + TEST_TIMEOUT_TICKS;
    while sched::finished_threads() < finished_before + count {
        if pit::ticks() >= deadline {
            return false;
        }
        cpu::hlt();
    }
    true
}
