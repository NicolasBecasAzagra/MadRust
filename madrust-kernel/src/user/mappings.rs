// SPDX-License-Identifier: GPL-2.0-only
//! The static table of user mappings and the lock that guards it.
//!
//! The record, the lookup and the range edits live in [`madrust_mm::mapping`],
//! where host unit tests cover them without hardware. This module adds the
//! storage the kernel needs: one fixed table and an interrupt safe lock shared
//! by the mapping system calls and the page fault handler.
//!
//! The page fault handler runs with interrupts already disabled; taking the
//! lock there only saves and restores the interrupt flag and then spins, which
//! cannot deadlock because no other path takes the lock with interrupts on and
//! then blocks. Every edit is a short, allocation-free walk of the table.

use core::cell::UnsafeCell;

use madrust_mm::mapping::{Mapping, MappingError, MappingTable};

use crate::sync::IrqSpinLock;

/// Slots in the table.
///
/// The workload needs one mapping, the runtime a handful of stack mappings, and
/// a split on `munmap` needs a spare slot, so the table leaves a wide margin.
pub const CAPACITY: usize = 32;

struct Storage(UnsafeCell<MappingTable<CAPACITY>>);

// SAFETY: the table is only reached through [`with`], which holds [`LOCK`] for
// the duration of the call.
unsafe impl Sync for Storage {}

static MAPPINGS: Storage = Storage(UnsafeCell::new(MappingTable::new()));

/// Guards the table.
static LOCK: IrqSpinLock = IrqSpinLock::new("user mappings");

/// Run `f` with the mapping table.
fn with<R>(f: impl FnOnce(&mut MappingTable<CAPACITY>) -> R) -> R {
    let _guard = LOCK.lock();
    // SAFETY: every access goes through this function, which holds the lock, so
    // the mutable borrow cannot overlap another one.
    let table = unsafe { &mut *MAPPINGS.0.get() };
    f(table)
}

/// Record a new anonymous mapping.
pub fn record(base: u64, length: u64, protection: u64) -> Result<(), MappingError> {
    with(|table| table.insert(Mapping::anonymous(base, length, protection)))
}

/// Replace `[base, base + length)` with one new anonymous mapping.
///
/// The edit runs on a copy of the table, so a failure leaves the old records
/// untouched even when the range would need a split and the table is full.
pub fn replace(base: u64, length: u64, protection: u64) -> Result<(), MappingError> {
    with(|table| {
        let mut next = *table;
        next.remove_range(base, base.saturating_add(length))?;
        next.insert(Mapping::anonymous(base, length, protection))?;
        *table = next;
        Ok(())
    })
}

/// Remove every record of `[base, base + length)`.
pub fn remove(base: u64, length: u64) -> Result<(), MappingError> {
    with(|table| table.remove_range(base, base.saturating_add(length)))
}

/// Change the protection of `[base, base + length)`.
pub fn protect(base: u64, length: u64, protection: u64) -> Result<(), MappingError> {
    with(|table| table.protect(base, base.saturating_add(length), protection))
}

/// Record that contains `address`, if any.
pub fn find(address: u64) -> Option<Mapping> {
    with(|table| table.find(address))
}

/// `true` when any record overlaps `[base, base + length)`.
pub fn overlaps(base: u64, length: u64) -> bool {
    with(|table| table.overlaps(base, length))
}

/// Number of slots in use and in the table.
pub fn usage() -> (usize, usize) {
    with(|table| (table.count(), table.capacity()))
}
