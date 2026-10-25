// SPDX-License-Identifier: GPL-2.0-only
//! Local APIC and its timer (milestone E3b).
//!
//! The local APIC is the interrupt controller inside each processor. The boot
//! processor calibrates its timer against the PIT once, and every processor
//! then programs the same count, so each processor has its own one millisecond
//! tick without another global clock.
//!
//! The register window is memory mapped at the physical address the
//! `IA32_APIC_BASE` MSR names, usually `0xFEE0_0000`. The direct map covers it,
//! so no page table change is needed.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use madrust_arch::cpu;
use madrust_mm::{phys_to_virt, DIRECT_MAP_SPAN};

use crate::pit;
use crate::vectors::{LAPIC_TIMER_VECTOR, SPURIOUS_VECTOR};

/// MSR that holds the local APIC base and enable bits.
const MSR_APIC_BASE: u32 = 0x1b;
/// Base used when the firmware leaves the MSR empty.
const DEFAULT_APIC_BASE: u64 = 0xfee0_0000;
/// The APIC is enabled when this bit is set.
const APIC_ENABLE: u64 = 1 << 11;
/// x2APIC mode is not used: the startup sequence addresses xAPIC identifiers.
const X2APIC_ENABLE: u64 = 1 << 10;
/// Base address mask of the MSR.
const APIC_BASE_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Local APIC register offsets.
const REG_ID: u32 = 0x020;
const REG_EOI: u32 = 0x0b0;
const REG_SPURIOUS: u32 = 0x0f0;
const REG_TIMER: u32 = 0x320;
const REG_THERMAL: u32 = 0x330;
const REG_PERFORMANCE: u32 = 0x340;
const REG_LINT0: u32 = 0x350;
const REG_LINT1: u32 = 0x360;
const REG_ERROR: u32 = 0x370;
const REG_TIMER_INITIAL: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3e0;
const REG_ICR_LOW: u32 = 0x300;
const REG_ICR_HIGH: u32 = 0x310;

/// Task priority register, cleared so every interrupt is delivered.
const REG_TPR: u32 = 0x080;

/// Mask bit of a local vector table entry.
const LVT_MASK: u32 = 1 << 16;
/// Periodic mode bit of the timer LVT entry.
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
/// Interrupt command register: assert the level.
const ICR_ASSERT: u32 = 1 << 14;
/// Interrupt command register: delivery is complete when this bit clears.
const ICR_DELIVERY_PENDING: u32 = 1 << 12;
/// Delivery mode 5: INIT.
const ICR_INIT: u32 = 5 << 8;
/// Delivery mode 6: start up (SIPI).
const ICR_STARTUP: u32 = 6 << 8;

/// Timer divide configuration value for a divide by 16.
const DIVIDE_BY_16: u32 = 0b011;

/// PIC ticks the calibration averages over, 100 ms at [`pit::TICK_HZ`].
const CALIBRATION_TICKS: u64 = 10;

/// Local APIC timer ticks in one millisecond, zero before calibration.
static TIMER_COUNT: AtomicU32 = AtomicU32::new(0);

/// `true` when the boot processor took preemption from the PIT.
static TIMER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Read one register of the local APIC.
fn read(offset: u32) -> u32 {
    let address = base() + u64::from(offset);
    // SAFETY: `base` is the register window the firmware described, page
    // aligned and inside the direct map. A 32-bit volatile read has no side
    // effect beyond the register semantics.
    unsafe { core::ptr::read_volatile(phys_to_virt(address) as *const u32) }
}

/// Write one register of the local APIC.
fn write(offset: u32, value: u32) {
    let address = base() + u64::from(offset);
    // SAFETY: same window as [`read`]. The register accepts a full 32-bit
    // value and the write is the only way to command the controller.
    unsafe { core::ptr::write_volatile(phys_to_virt(address) as *mut u32, value) };
}

/// Physical base of the register window.
fn base() -> u64 {
    let configured = cpu::read_msr(MSR_APIC_BASE) & APIC_BASE_MASK;
    if configured == 0 {
        DEFAULT_APIC_BASE
    } else {
        configured
    }
}

/// Enable the local APIC of the calling processor.
///
/// The x2APIC mode is turned off when the firmware enabled it, because the
/// startup sequence and the interrupt command register below use the xAPIC
/// memory mapped format.
pub fn init() {
    let current = cpu::read_msr(MSR_APIC_BASE);
    let base = if current & APIC_BASE_MASK == 0 {
        DEFAULT_APIC_BASE
    } else {
        current & APIC_BASE_MASK
    };
    if base + 0x1000 > DIRECT_MAP_SPAN {
        return;
    }
    // SAFETY: the MSR exists on every x86_64 CPU with a local APIC, and the
    // value keeps the base the firmware selected while enabling the xAPIC
    // window the registers above expect.
    unsafe {
        cpu::write_msr(
            MSR_APIC_BASE,
            (current & !(X2APIC_ENABLE | APIC_BASE_MASK)) | base | APIC_ENABLE,
        );
    }

    write(REG_TPR, 0);
    let spurious = read(REG_SPURIOUS);
    write(
        REG_SPURIOUS,
        (spurious & !0xff) | u32::from(SPURIOUS_VECTOR) | 0x100,
    );

    // LINT0 stays in ExtINT mode so the 8259 can still deliver the PIT while
    // the timer calibrates. Once the input output APIC takes the line over the
    // 8259 is masked. LINT1 becomes an NMI input, as the PC wires it.
    write(REG_LINT0, 7 << 8);
    write(REG_LINT1, 4 << 8);

    // Mask the local vector table entries this kernel does not use, with the
    // timer masked until the calibration has produced a count.
    for register in [REG_TIMER, REG_THERMAL, REG_PERFORMANCE, REG_ERROR] {
        let value = read(register);
        write(register, value | LVT_MASK);
    }
}

/// Identifier of the local APIC of the calling processor.
pub fn id() -> u32 {
    read(REG_ID) >> 24
}

/// Acknowledge the interrupt that is being serviced.
pub fn eoi() {
    write(REG_EOI, 0);
}

/// Calibrate the timer against the PIT and store the count for one millisecond.
///
/// Interrupts are enabled for the wait and left disabled afterwards. The
/// returned count is zero when the counter cannot be measured.
pub fn calibrate() -> u32 {
    write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
    write(REG_TIMER, u32::from(LAPIC_TIMER_VECTOR) | LVT_MASK);
    write(REG_TIMER_INITIAL, u32::MAX);

    let start = pit::ticks();
    cpu::enable_interrupts();
    while pit::ticks() < start + CALIBRATION_TICKS {
        cpu::hlt();
    }
    let remaining = read(REG_TIMER_CURRENT);
    cpu::disable_interrupts();

    let elapsed = u64::from(u32::MAX - remaining);
    let per_tick = elapsed / CALIBRATION_TICKS;
    let per_ms = per_tick / (pit::IRQ_HZ / pit::TICK_HZ);
    let count = u32::try_from(per_ms).unwrap_or(0);
    TIMER_COUNT.store(count, Ordering::Release);
    count
}

/// Count of timer ticks in one millisecond, zero when not calibrated.
pub fn timer_count() -> u32 {
    TIMER_COUNT.load(Ordering::Acquire)
}

/// Start the periodic timer of the calling processor.
///
/// Returns `false` when the boot processor could not calibrate the timer; the
/// caller then keeps the PIT as the tick source.
pub fn start_timer() -> bool {
    let count = timer_count();
    if count == 0 {
        return false;
    }
    write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
    write(
        REG_TIMER,
        u32::from(LAPIC_TIMER_VECTOR) | LVT_TIMER_PERIODIC,
    );
    write(REG_TIMER_INITIAL, count);
    TIMER_ACTIVE.store(true, Ordering::Release);
    true
}

/// `true` when the local APIC timer drives preemption on the boot processor.
pub fn timer_active() -> bool {
    TIMER_ACTIVE.load(Ordering::Acquire)
}

/// Wait until the interrupt command register accepts another command.
fn wait_for_delivery() {
    let mut spins = 0u32;
    while read(REG_ICR_LOW) & ICR_DELIVERY_PENDING != 0 {
        core::hint::spin_loop();
        spins = spins.wrapping_add(1);
        if spins == u32::MAX {
            break;
        }
    }
}

/// Send one fixed vector to a processor.
pub fn send_ipi(destination: u32, vector: u8) {
    wait_for_delivery();
    write(REG_ICR_HIGH, destination << 24);
    write(REG_ICR_LOW, u32::from(vector) | ICR_ASSERT);
}

/// Send an INIT inter processor interrupt to a processor.
pub fn send_init(destination: u32) {
    wait_for_delivery();
    write(REG_ICR_HIGH, destination << 24);
    write(REG_ICR_LOW, ICR_INIT | ICR_ASSERT);
}

/// Send a start up inter processor interrupt to a processor.
///
/// `page` is the real mode start page, so the processor begins at
/// `page << 12` with `CS = page << 8`.
pub fn send_startup(destination: u32, page: u8) {
    wait_for_delivery();
    write(REG_ICR_HIGH, destination << 24);
    write(REG_ICR_LOW, ICR_STARTUP | ICR_ASSERT | u32::from(page));
}

/// Send one fixed vector to every processor except the caller.
pub fn broadcast_ipi(vector: u8) {
    wait_for_delivery();
    write(REG_ICR_HIGH, 0);
    write(REG_ICR_LOW, u32::from(vector) | ICR_ASSERT | (3 << 18));
}
