// SPDX-License-Identifier: GPL-2.0-only
//! 8253/8254 programmable interval timer.

use core::sync::atomic::{AtomicU64, Ordering};

use madrust_arch::port;

/// Tick rate in hertz.
pub const TICK_HZ: u64 = 100;
/// Input clock of the PIT.
const PIT_INPUT_HZ: u32 = 1_193_182;
/// Channel 0, lo/hi byte access, mode 3 (square wave), binary.
const CHANNEL0_MODE3: u8 = 0x34;
const CHANNEL0_COMMAND: u16 = 0x43;
const CHANNEL0_DATA: u16 = 0x40;
/// Divisor for a 100 Hz tick: 1193182 / 100 = 11931.
const DIVISOR: u16 = (PIT_INPUT_HZ / TICK_HZ as u32) as u16;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Program channel 0 to raise IRQ0 at [`TICK_HZ`].
pub fn init() {
    port::outb(CHANNEL0_COMMAND, CHANNEL0_MODE3);
    port::outb(CHANNEL0_DATA, DIVISOR as u8);
    port::outb(CHANNEL0_DATA, (DIVISOR >> 8) as u8);
}

/// Count one timer interrupt.
pub fn record_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Number of timer interrupts observed since boot.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
