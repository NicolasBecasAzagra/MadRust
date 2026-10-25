// SPDX-License-Identifier: GPL-2.0-only
//! The static table of resident ranges and the lock that guards it.
//!
//! The record and the range arithmetic live in [`madrust_mm::pin`], where host
//! unit tests cover them without hardware. This module adds the storage the
//! kernel needs: one fixed table and an interrupt safe lock shared by the
//! `mlock`, `munlock` and `munmap` paths.
//!
//! `munmap` runs with interrupts disabled from a system call and with them
//! already disabled from the page fault handler; taking the lock there only
//! saves and restores the interrupt flag and then spins, which cannot deadlock
//! because no path holds the lock across a block.

use core::cell::UnsafeCell;

use madrust_mm::pin::{Pin, PinError, PinTable};

use crate::sync::IrqSpinLock;

/// Slots in the table.
///
/// A pin taken from a large page covers 2 MiB with one record, so the runtime
/// pins tens of megabytes with a handful of slots. The margin covers a mapping
/// that was faulted as 4 KiB pages first: the synthetic model is a few MiB, so
/// every page of it still fits when no large page is available.
pub const CAPACITY: usize = 2048;

struct Storage(UnsafeCell<PinTable<CAPACITY>>);

// SAFETY: the table is only reached through [`with`], which holds [`LOCK`] for
// the duration of the call.
unsafe impl Sync for Storage {}

static PINS: Storage = Storage(UnsafeCell::new(PinTable::new()));

/// Guards the table.
static LOCK: IrqSpinLock = IrqSpinLock::new("user pins");

/// Run `f` with the pin table.
fn with<R>(f: impl FnOnce(&mut PinTable<CAPACITY>) -> R) -> R {
    let _guard = LOCK.lock();
    // SAFETY: every access goes through this function, which holds the lock, so
    // the mutable borrow cannot overlap another one.
    let table = unsafe { &mut *PINS.0.get() };
    f(table)
}

/// Start a pin transaction for one `mlock` call.
pub fn begin() -> u32 {
    with(|table| table.begin())
}

/// Add a pin to an open transaction.
pub fn insert(pin: Pin) -> Result<(), PinError> {
    with(|table| table.insert(pin))
}

/// Keep every pin of a completed transaction.
pub fn commit(generation: u32) {
    with(|table| table.commit(generation));
}

/// Remove every pin of a failed transaction.
pub fn discard(generation: u32) {
    with(|table| table.discard(generation));
}

/// Remove and return the first pinned part of `[base, end)`.
pub fn claim(base: u64, end: u64) -> Result<Option<Pin>, PinError> {
    with(|table| table.claim(base, end))
}

/// `true` when one pin covers the whole range.
pub fn covers(base: u64, length: u64) -> bool {
    with(|table| table.covers(base, length))
}

/// `true` when any pin overlaps the range.
pub fn overlaps(base: u64, length: u64) -> bool {
    with(|table| table.overlaps(base, length))
}

/// `true` when a pin holds a frame of `[physical, physical + size)`.
pub fn holds_physical(physical: u64, size: u64) -> bool {
    with(|table| table.holds_physical(physical, size))
}

/// Number of pages the program pinned.
pub fn locked_pages() -> u64 {
    with(|table| table.locked_pages())
}

/// Number of bytes the program pinned.
pub fn locked_bytes() -> u64 {
    with(|table| table.locked_bytes())
}
