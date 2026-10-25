// SPDX-License-Identifier: GPL-2.0-only
//! High resolution wall clock.
//!
//! The PIT only interrupts at 100 Hz, so a clock built from it quantises every
//! measurement to 10 ms. `clock_gettime` would then make the benchmark useless.
//! The time stamp counter ticks at the CPU frequency and is readable at any
//! moment, so the kernel calibrates it against the PIT once during boot and
//! reports seconds and nanoseconds from it afterwards.
//!
//! The PIT is still the authority for scheduling and futex timeouts: those work
//! in whole ticks. Only the clock the program reads uses the finer counter.

use core::sync::atomic::{AtomicU64, Ordering};

use madrust_arch::cpu;

use crate::pit;

/// Timer ticks the calibration averages over, 100 ms at [`pit::TICK_HZ`].
const CALIBRATION_TICKS: u64 = 10;

/// Number of nanoseconds in one second, as a `u128` for the division.
const NANOS_PER_SECOND: u128 = 1_000_000_000;

/// Calibrated time stamp counter rate, zero when the counter is unusable.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);

/// Counter value at which the clock reports zero.
static BASE_TSC: AtomicU64 = AtomicU64::new(0);

/// Calibrate the counter against the PIT.
///
/// Called once after [`pit::init`] and before any program runs. Interrupts are
/// enabled for the wait and left disabled afterwards, which is the state the
/// boot path expects at that point.
pub fn init() -> u64 {
    let start_ticks = pit::ticks();
    let start_tsc = cpu::read_tsc();

    cpu::enable_interrupts();
    while pit::ticks() < start_ticks + CALIBRATION_TICKS {
        cpu::hlt();
    }
    let end_tsc = cpu::read_tsc();
    cpu::disable_interrupts();

    let cycles = u128::from(end_tsc.wrapping_sub(start_tsc));
    let hertz = cycles * u128::from(pit::TICK_HZ) / u128::from(CALIBRATION_TICKS);
    let Ok(hertz) = u64::try_from(hertz) else {
        return 0;
    };
    BASE_TSC.store(start_tsc, Ordering::Release);
    TSC_HZ.store(hertz, Ordering::Release);
    hertz
}

/// Calibrated time stamp counter rate in hertz, zero when unusable.
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Acquire)
}

/// Convert a distance in time stamp counter cycles to microseconds.
///
/// The scheduler self test measures its own latency with this, so it shares the
/// calibration the clock system calls use. Cycles from a counter that was not
/// calibrated read as zero.
pub fn microseconds(cycles: u64) -> u64 {
    let hertz = tsc_hz();
    if hertz == 0 {
        return 0;
    }
    let microseconds = u128::from(cycles) * 1_000_000 / u128::from(hertz);
    u64::try_from(microseconds).unwrap_or(u64::MAX)
}

/// Seconds and nanoseconds since the clock base.
///
/// Falls back to the PIT tick count when the counter was not calibrated, so
/// the call always answers even on a CPU without a usable TSC.
pub fn now() -> (u64, u64) {
    let hertz = TSC_HZ.load(Ordering::Acquire);
    if hertz == 0 {
        let ticks = pit::ticks();
        let seconds = ticks / pit::TICK_HZ;
        let nanoseconds = (ticks % pit::TICK_HZ) * (1_000_000_000 / pit::TICK_HZ);
        return (seconds, nanoseconds);
    }

    let cycles = cpu::read_tsc().wrapping_sub(BASE_TSC.load(Ordering::Acquire));
    let nanoseconds = u128::from(cycles) * NANOS_PER_SECOND / u128::from(hertz);
    let seconds = u64::try_from(nanoseconds / NANOS_PER_SECOND).unwrap_or(u64::MAX);
    let nanoseconds = u64::try_from(nanoseconds % NANOS_PER_SECOND).unwrap_or(0);
    (seconds, nanoseconds)
}
