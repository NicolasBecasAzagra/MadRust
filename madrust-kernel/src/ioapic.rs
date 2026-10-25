// SPDX-License-Identifier: GPL-2.0-only
//! Input output APIC (milestone E3b).
//!
//! The input output APIC carries the device interrupts that do not go to a
//! local APIC directly. On the machines this milestone runs on it takes over
//! the PIT line from the 8259: the PIT is routed to the vector in
//! [`crate::vectors::PIT_IOAPIC_VECTOR`], delivered to the boot processor.
//!
//! The register window is memory mapped; the direct map covers it, so the
//! controller is reached without a page table change.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use madrust_mm::{phys_to_virt, DIRECT_MAP_SPAN};

/// Offset of the register selector inside the window.
const IOREGSEL: u64 = 0x00;
/// Offset of the register window.
const IOWIN: u64 = 0x10;
/// First redirection entry.
const REDIRECTION_BASE: u32 = 0x10;
/// Mask bit of a redirection entry.
const REDIRECTION_MASK: u32 = 1 << 16;
/// Level trigger bit.
const REDIRECTION_LEVEL: u32 = 1 << 15;
/// Active low bit.
const REDIRECTION_LOW: u32 = 1 << 13;
/// Number of redirection entries the controller may implement.
const MAX_ENTRIES: u32 = 24;
/// Version register, whose high half holds one less than the entry count.
const REG_VERSION: u32 = 0x01;

/// Physical base of the input output APIC window.
static BASE: AtomicU64 = AtomicU64::new(0);
/// First global system interrupt of the controller.
static GSI_BASE: AtomicU32 = AtomicU32::new(0);
/// `true` after [`init`] succeeded.
static READY: AtomicBool = AtomicBool::new(false);

/// Read one 32-bit register of the controller.
fn read(register: u32) -> u32 {
    let base = BASE.load(Ordering::Acquire);
    // SAFETY: `base` is a register window below the direct map span and the
    // selector value selects a 32-bit register.
    unsafe {
        let select = phys_to_virt(base + IOREGSEL) as *mut u32;
        let window = phys_to_virt(base + IOWIN) as *const u32;
        core::ptr::write_volatile(select, register);
        core::ptr::read_volatile(window)
    }
}

/// Write one 32-bit register of the controller.
fn write(register: u32, value: u32) {
    let base = BASE.load(Ordering::Acquire);
    // SAFETY: same window and register selection as [`read`].
    unsafe {
        let select = phys_to_virt(base + IOREGSEL) as *mut u32;
        let window = phys_to_virt(base + IOWIN) as *mut u32;
        core::ptr::write_volatile(select, register);
        core::ptr::write_volatile(window, value);
    }
}

/// Write one redirection entry.
fn write_entry(index: u32, low: u32, high: u32) {
    let register = REDIRECTION_BASE + index * 2;
    write(register, low);
    write(register + 1, high);
}

/// Read the high half of one redirection entry.
fn high_entry(index: u32) -> u32 {
    read(REDIRECTION_BASE + index * 2 + 1)
}

/// Index of `gsi` in this controller, when it owns the line.
fn index(gsi: u32) -> Option<u32> {
    if !READY.load(Ordering::Acquire) {
        return None;
    }
    let index = gsi.checked_sub(GSI_BASE.load(Ordering::Acquire))?;
    (index < MAX_ENTRIES).then_some(index)
}

/// Mask every redirection entry.
///
/// The reset state is not defined for every controller, so the boot path masks
/// the whole table before it routes the one line it needs.
pub fn init(address: u64, gsi_base: u32) -> bool {
    if address == 0 || address + 0x20 > DIRECT_MAP_SPAN {
        return false;
    }
    BASE.store(address, Ordering::Release);
    GSI_BASE.store(gsi_base, Ordering::Release);
    READY.store(true, Ordering::Release);

    let version = read(REG_VERSION);
    let entries = ((version >> 16) & 0xff).min(MAX_ENTRIES);
    for entry in 0..entries {
        write_entry(entry, REDIRECTION_MASK, 0);
    }
    true
}

/// Route one global system interrupt to a vector.
///
/// `level` selects level triggered delivery, `low` active low polarity, both as
/// the MPS INTI flags of the MADT describe them. Delivery goes to `destination`
/// with physical destination mode.
pub fn route(gsi: u32, vector: u8, destination: u32, level: bool, low: bool) {
    let Some(entry) = index(gsi) else {
        return;
    };
    let mut low_bits = u32::from(vector);
    if level {
        low_bits |= REDIRECTION_LEVEL;
    }
    if low {
        low_bits |= REDIRECTION_LOW;
    }
    let high = (high_entry(entry) & 0x00ff_ffff) | (destination << 24);
    write_entry(entry, low_bits, high);
}

/// Decode the MPS INTI flags of a timer interrupt source override.
///
/// Returns the level and active low bits the redirection entry needs. An
/// override value of zero means "conform to the bus", which for the ISA timer
/// line is edge triggered and active high.
pub fn mps_flags(flags: u16) -> (bool, bool) {
    let low = flags & 0b11 == 0b11;
    let level = (flags >> 2) & 0b11 == 0b11;
    (level, low)
}
