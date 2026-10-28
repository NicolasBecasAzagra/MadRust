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
//!
//! The table never travels through the stack: a copy of it is larger than the
//! sixteen kilobyte kernel stack of a thread, and these paths run on that stack
//! when the program enters the kernel from ring 3. Callers therefore look at
//! the records through [`find`] instead of taking a copy, and the one edit that
//! needs a working copy keeps it in static storage.

use core::cell::UnsafeCell;

use madrust_mm::mapping::{Mapping, MappingError, MappingTable};

use crate::sync::IrqSpinLock;

/// Slots in the table.
///
/// The workload needs one mapping and the runtime a handful of stack mappings,
/// but a program that uses the standard library asks the kernel for one
/// mapping per allocation: the allocator of Rust reserves every block with
/// its own `mmap`, and a hundred small allocations are a hundred mappings. A
/// table of thirty two slots was full before the program had read a file, so
/// the table has room for a thousand and the lookup that scans it is the
/// price.
pub const CAPACITY: usize = 1024;

struct Storage(UnsafeCell<MappingTable<CAPACITY>>);

// SAFETY: the table is only reached through [`with`], which holds [`LOCK`] for
// the duration of the call.
unsafe impl Sync for Storage {}

static MAPPINGS: Storage = Storage(UnsafeCell::new(MappingTable::new()));

/// Working copy of the table for the edits that have to be atomic.
///
/// It lives in static storage and not on the stack: the table holds a thousand
/// records, so it is larger than the kernel stack a system call runs on, and a
/// copy of it on that stack would write over whatever the linker placed after
/// the stack.
struct Scratch(UnsafeCell<MappingTable<CAPACITY>>);

// SAFETY: the scratch is only borrowed from [`replace`], which runs inside
// [`with`] and therefore holds [`LOCK`] for the whole edit.
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch(UnsafeCell::new(MappingTable::new()));

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

/// Record a new mapping.
pub fn record(mapping: Mapping) -> Result<(), MappingError> {
    with(|table| table.insert(mapping))
}

/// Replace `[base, base + length)` with one new mapping.
///
/// The edit runs on a working copy, so a failure leaves the old records
/// untouched even when the range would need a split and the table is full. The
/// copy is the static [`SCRATCH`], never a local: the table is larger than the
/// kernel stack.
pub fn replace(base: u64, length: u64, mapping: Mapping) -> Result<(), MappingError> {
    with(|table| {
        // SAFETY: `with` holds `LOCK`, and `SCRATCH` is only read or written
        // here, so this exclusive borrow cannot overlap another one.
        let next = unsafe { &mut *SCRATCH.0.get() };
        *next = *table;
        next.remove_range(base, base.saturating_add(length))?;
        next.insert(mapping)?;
        *table = *next;
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

/// Set or clear the large page hint of `[base, base + length)`.
pub fn advise(base: u64, length: u64, huge: bool) -> Result<(), MappingError> {
    with(|table| table.advise(base, base.saturating_add(length), huge))
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
