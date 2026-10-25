// SPDX-License-Identifier: GPL-2.0-only
//! 8253/8254 programmable interval timer.
//!
//! The timer serves two periods. The accounting tick is the 10 ms unit every
//! existing user sees through [`ticks`]: futex timeouts, the clock fallback and
//! the boot checks count in it. The interrupt itself runs ten times faster, so
//! the scheduler can slice a tick into 1 ms quanta and a time slice can be
//! shorter than a whole tick.

use core::sync::atomic::{AtomicU64, Ordering};

use madrust_arch::port;

/// Accounting tick rate in hertz.
pub const TICK_HZ: u64 = 100;
/// Timer interrupts per second, the quantum the scheduler counts slices in.
pub const IRQ_HZ: u64 = 1_000;
/// Timer interrupts that make up one accounting tick.
const QUANTA_PER_TICK: u64 = IRQ_HZ / TICK_HZ;
/// Input clock of the PIT.
const PIT_INPUT_HZ: u32 = 1_193_182;
/// Channel 0, lo/hi byte access, mode 3 (square wave), binary.
const CHANNEL0_MODE3: u8 = 0x34;
const CHANNEL0_COMMAND: u16 = 0x43;
const CHANNEL0_DATA: u16 = 0x40;
/// Divisor for [`IRQ_HZ`]: 1193182 / 1000 = 1193.
const DIVISOR: u16 = (PIT_INPUT_HZ / IRQ_HZ as u32) as u16;

static TICKS: AtomicU64 = AtomicU64::new(0);
static QUANTA: AtomicU64 = AtomicU64::new(0);

/// Program channel 0 to raise IRQ0 at [`IRQ_HZ`].
pub fn init() {
    port::outb(CHANNEL0_COMMAND, CHANNEL0_MODE3);
    port::outb(CHANNEL0_DATA, DIVISOR as u8);
    port::outb(CHANNEL0_DATA, (DIVISOR >> 8) as u8);
}

/// Count one timer interrupt and advance the accounting tick with it.
pub fn record_tick() {
    let quanta = QUANTA.fetch_add(1, Ordering::Relaxed) + 1;
    if quanta.is_multiple_of(QUANTA_PER_TICK) {
        TICKS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Number of 10 ms accounting ticks observed since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
