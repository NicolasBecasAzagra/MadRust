// SPDX-License-Identifier: GPL-2.0-only
//! Resident user ranges and the fixed-size table that holds them.
//!
//! `mlock` promises that a range stays in physical memory until the program
//! unlocks it. The kernel has no swap, so the promise reduces to ownership: the
//! frames of a pinned range must not go back to the buddy allocator while the
//! pin is held, even when the mapping that brought them in is removed. This
//! module keeps that bookkeeping pure: a pin records the virtual range, the
//! physical allocation behind it, the buddy order it was taken at and whether
//! the mapping owns that memory. The lookup and the range edits never allocate
//! and never touch hardware, so the host tests cover the logic the kernel runs
//! on the `mlock` and `munlock` paths.
//!
//! A pin is page granular. A pin taken from a 2 MiB leaf keeps the order of the
//! block, so a partial unlock of the block leaves the allocation intact and the
//! last page of the pin returns the whole block at that order. A read only file
//! page is backed by the reserved initramfs, so its pin is marked as not owned
//! and is never given back.

use crate::paging::PAGE_SIZE;

/// One pinned user range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    /// First virtual address of the range, always page aligned.
    pub base: u64,
    /// Length in bytes, always a multiple of the page size.
    pub length: u64,
    /// Physical base of the allocation behind the range.
    ///
    /// For an order 0 pin this is the frame itself. For a pin taken from a
    /// large page it is the base of the 2 MiB block, even when the pin covers
    /// only a page of it.
    pub physical: u64,
    /// Buddy order the allocation was taken at.
    pub order: u8,
    /// `true` when the pin owns the physical memory and must return it.
    ///
    /// A 4 KiB page of a file mapping borrows the reserved initramfs, so it is
    /// not owned. An anonymous frame and a copied large file block are owned.
    pub owned: bool,
    /// Rollback token of the `mlock` call that inserted the pin, or zero when
    /// the call completed. It makes a failed lock remove only its own pins.
    pub generation: u32,
}

impl Pin {
    /// One past the last virtual address of the range.
    pub const fn end(&self) -> u64 {
        self.base.saturating_add(self.length)
    }

    /// `true` when `address` falls inside the range.
    pub fn contains(&self, address: u64) -> bool {
        address >= self.base && address < self.end()
    }

    /// Size in bytes of the physical allocation behind the pin.
    pub const fn allocation_size(&self) -> u64 {
        PAGE_SIZE << self.order
    }

    /// `true` when the physical allocation of the pin overlaps `[base, base + size)`.
    pub const fn overlaps_physical(&self, base: u64, size: u64) -> bool {
        self.physical < base.saturating_add(size)
            && base < self.physical.saturating_add(self.allocation_size())
    }

    /// Number of pages the range covers.
    pub const fn pages(&self) -> u64 {
        self.length / PAGE_SIZE
    }
}

/// Why the table refused an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinError {
    /// The table has no free slot for a new record or a split.
    Full,
    /// The range is empty or wraps around the address space.
    InvalidRange,
}

/// Fixed-size table of pinned ranges.
///
/// The table keeps its records in insertion order and a lookup scans the slots.
/// The number of pins is bounded by [`PinTable`]'s capacity, which turns an
/// exhausted pin budget into a recoverable error instead of an allocation on
/// the `mlock` path.
pub struct PinTable<const CAPACITY: usize> {
    slots: [Option<Pin>; CAPACITY],
    next_generation: u32,
}

impl<const CAPACITY: usize> Default for PinTable<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> PinTable<CAPACITY> {
    /// An empty table.
    pub const fn new() -> Self {
        Self {
            slots: [None; CAPACITY],
            next_generation: 0,
        }
    }

    /// Number of slots in the table.
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    /// Number of slots in use.
    pub fn count(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Pin that contains `address`, if any.
    pub fn find(&self, address: u64) -> Option<Pin> {
        self.slots
            .iter()
            .flatten()
            .copied()
            .find(|pin| pin.contains(address))
    }

    /// `true` when one pin covers the whole `[base, base + length)` range.
    pub fn covers(&self, base: u64, length: u64) -> bool {
        let Some(end) = base.checked_add(length) else {
            return false;
        };
        self.slots
            .iter()
            .flatten()
            .any(|pin| pin.base <= base && end <= pin.end())
    }

    /// `true` when any pin overlaps `[base, base + length)`.
    pub fn overlaps(&self, base: u64, length: u64) -> bool {
        let end = base.saturating_add(length);
        self.slots
            .iter()
            .flatten()
            .any(|pin| pin.base < end && base < pin.end())
    }

    /// `true` when any pin holds a frame overlapping `[physical, physical + size)`.
    pub fn holds_physical(&self, physical: u64, size: u64) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|pin| pin.overlaps_physical(physical, size))
    }

    /// Add a pin.
    pub fn insert(&mut self, pin: Pin) -> Result<(), PinError> {
        if pin.length == 0 || pin.base.checked_add(pin.length).is_none() {
            return Err(PinError::InvalidRange);
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
            return Err(PinError::Full);
        };
        *slot = Some(pin);
        Ok(())
    }

    /// Remove and return the first part of a pin that falls inside `[base, end)`.
    ///
    /// A pin that only partly overlaps is trimmed, and a pin that covers the
    /// range on both sides is split, so the caller can walk a partial unlock
    /// one record at a time. The returned record keeps the physical allocation
    /// and the order of the pin it came from, which is what the caller needs to
    /// decide whether the whole allocation is now free.
    pub fn claim(&mut self, base: u64, end: u64) -> Result<Option<Pin>, PinError> {
        if base >= end {
            return Ok(None);
        }
        let Some(index) = self
            .slots
            .iter()
            .position(|slot| slot.is_some_and(|pin| pin.base < end && base < pin.end()))
        else {
            return Ok(None);
        };
        let Some(pin) = self.slots[index] else {
            return Ok(None);
        };
        let pin_end = pin.end();

        if base <= pin.base && pin_end <= end {
            self.slots[index] = None;
            return Ok(Some(pin));
        }
        if pin.base < base && end < pin_end {
            let Some(free) = self.slots.iter().position(|slot| slot.is_none()) else {
                return Err(PinError::Full);
            };
            self.slots[index] = Some(Pin {
                length: base - pin.base,
                ..pin
            });
            self.slots[free] = Some(Pin {
                base: end,
                length: pin_end - end,
                ..pin
            });
            return Ok(Some(Pin {
                base,
                length: end - base,
                ..pin
            }));
        }
        if pin.base < base {
            self.slots[index] = Some(Pin {
                length: base - pin.base,
                ..pin
            });
            return Ok(Some(Pin {
                base,
                length: pin_end - base,
                ..pin
            }));
        }
        self.slots[index] = Some(Pin {
            base: end,
            length: pin_end - end,
            ..pin
        });
        Ok(Some(Pin {
            length: end - pin.base,
            ..pin
        }))
    }

    /// Start a pin transaction and return its token.
    ///
    /// Zero is never a token, so a committed pin cannot match a later call.
    pub fn begin(&mut self) -> u32 {
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        self.next_generation
    }

    /// Mark every pin of a completed transaction as permanent.
    pub fn commit(&mut self, generation: u32) {
        for slot in &mut self.slots {
            if let Some(pin) = *slot {
                if pin.generation == generation {
                    *slot = Some(Pin {
                        generation: 0,
                        ..pin
                    });
                }
            }
        }
    }

    /// Remove every pin of a failed transaction.
    pub fn discard(&mut self, generation: u32) {
        for slot in &mut self.slots {
            if slot.is_some_and(|pin| pin.generation == generation) {
                *slot = None;
            }
        }
    }

    /// Number of pages every pin covers together.
    pub fn locked_pages(&self) -> u64 {
        self.slots.iter().flatten().map(|pin| pin.pages()).sum()
    }

    /// Number of bytes every pin covers together.
    pub fn locked_bytes(&self) -> u64 {
        self.slots.iter().flatten().map(|pin| pin.length).sum()
    }

    /// Drop every pin.
    pub fn clear(&mut self) {
        self.slots = [None; CAPACITY];
    }
}

#[cfg(test)]
mod tests {
    use super::{Pin, PinError, PinTable};
    use crate::paging::{LARGE_PAGE_SIZE, PAGE_SIZE};

    /// One 4 KiB anonymous page.
    fn page(base: u64, physical: u64) -> Pin {
        Pin {
            base,
            length: PAGE_SIZE,
            physical,
            order: 0,
            owned: true,
            generation: 0,
        }
    }

    /// One 2 MiB anonymous block.
    fn block(base: u64, physical: u64) -> Pin {
        Pin {
            base,
            length: LARGE_PAGE_SIZE,
            physical,
            order: crate::LARGE_PAGE_ORDER,
            owned: true,
            generation: 0,
        }
    }

    #[test]
    fn finds_a_pin_inside_a_range() {
        let mut table = PinTable::<4>::new();
        table.insert(page(0x1000, 0x20_0000)).expect("pin fits");
        assert_eq!(table.find(0x1000).map(|pin| pin.physical), Some(0x20_0000));
        assert_eq!(table.find(0x1fff).map(|pin| pin.base), Some(0x1000));
        assert_eq!(table.find(0x2000), None);
    }

    #[test]
    fn covers_requires_the_whole_range() {
        let mut table = PinTable::<4>::new();
        table.insert(block(0x20_0000, 0x40_0000)).expect("pin fits");
        assert!(table.covers(0x20_0000, LARGE_PAGE_SIZE));
        assert!(table.covers(0x20_0000, PAGE_SIZE));
        assert!(!table.covers(0x21_0000, LARGE_PAGE_SIZE));
        assert!(!table.covers(0x1f_f000, PAGE_SIZE));
    }

    #[test]
    fn overlaps_reports_a_partial_cover() {
        let mut table = PinTable::<4>::new();
        table.insert(block(0x20_0000, 0x40_0000)).expect("pin fits");
        assert!(table.overlaps(0x20_0000, LARGE_PAGE_SIZE));
        assert!(table.overlaps(0x1f_f000, 2 * PAGE_SIZE));
        assert!(table.overlaps(0x3f_f000, 2 * PAGE_SIZE));
        assert!(!table.overlaps(0x1f_f000, PAGE_SIZE));
        assert!(!table.overlaps(0x40_0000, PAGE_SIZE));
    }

    #[test]
    fn reports_pages_and_bytes() {
        let mut table = PinTable::<4>::new();
        table.insert(page(0x1000, 0x20_0000)).expect("pin fits");
        table.insert(block(0x20_0000, 0x40_0000)).expect("pin fits");
        assert_eq!(table.locked_pages(), 1 + LARGE_PAGE_SIZE / PAGE_SIZE);
        assert_eq!(table.locked_bytes(), PAGE_SIZE + LARGE_PAGE_SIZE);
    }

    #[test]
    fn claim_removes_a_covered_pin() {
        let mut table = PinTable::<4>::new();
        table.insert(page(0x1000, 0x20_0000)).expect("pin fits");
        let removed = table.claim(0x1000, 0x2000).expect("range fits");
        assert_eq!(removed, Some(page(0x1000, 0x20_0000)));
        assert_eq!(table.count(), 0);
        assert_eq!(table.claim(0x1000, 0x2000).expect("range fits"), None);
    }

    #[test]
    fn claim_trims_a_pin_at_one_end() {
        let mut table = PinTable::<4>::new();
        table.insert(block(0x20_0000, 0x40_0000)).expect("pin fits");
        let removed = table
            .claim(0x20_0000, 0x20_0000 + PAGE_SIZE)
            .expect("range fits");
        assert_eq!(removed.map(|pin| pin.length), Some(PAGE_SIZE));
        assert_eq!(table.count(), 1);
        assert_eq!(
            table.find(0x20_0000 + PAGE_SIZE).map(|pin| pin.length),
            Some(LARGE_PAGE_SIZE - PAGE_SIZE)
        );
        assert_eq!(table.find(0x20_0000), None);
    }

    #[test]
    fn claim_splits_a_pin_in_the_middle() {
        let mut table = PinTable::<4>::new();
        table.insert(block(0x20_0000, 0x40_0000)).expect("pin fits");
        let removed = table
            .claim(0x20_0000 + PAGE_SIZE, 0x20_0000 + 2 * PAGE_SIZE)
            .expect("range fits");
        assert_eq!(removed.map(|pin| pin.length), Some(PAGE_SIZE));
        assert_eq!(table.count(), 2);
        assert_eq!(table.find(0x20_0000).map(|pin| pin.length), Some(PAGE_SIZE));
        assert_eq!(
            table
                .find(0x20_0000 + 2 * PAGE_SIZE)
                .map(|pin| pin.physical),
            Some(0x40_0000)
        );
    }

    #[test]
    fn a_hole_split_without_a_free_slot_reports_full() {
        let mut table = PinTable::<1>::new();
        table.insert(block(0x20_0000, 0x40_0000)).expect("pin fits");
        assert_eq!(
            table.claim(0x20_0000 + PAGE_SIZE, 0x20_0000 + 2 * PAGE_SIZE),
            Err(PinError::Full)
        );
        assert_eq!(table.count(), 1);
    }

    #[test]
    fn holds_physical_matches_the_allocation_of_a_large_pin() {
        let mut table = PinTable::<4>::new();
        table
            .insert(Pin {
                length: PAGE_SIZE,
                ..block(0x20_0000, 0x40_0000)
            })
            .expect("pin fits");
        assert!(table.holds_physical(0x40_0000, LARGE_PAGE_SIZE));
        assert!(table.holds_physical(0x41_0000, PAGE_SIZE));
        assert!(!table.holds_physical(0x60_0000, PAGE_SIZE));
        assert!(!table.holds_physical(0x40_0000, 0));
    }

    #[test]
    fn a_failed_transaction_discards_only_its_own_pins() {
        let mut table = PinTable::<4>::new();
        let stable = table.begin();
        table
            .insert(Pin {
                generation: stable,
                ..page(0x1000, 0x20_0000)
            })
            .expect("pin fits");
        table.commit(stable);

        let failed = table.begin();
        table
            .insert(Pin {
                generation: failed,
                ..page(0x2000, 0x30_0000)
            })
            .expect("pin fits");
        table.discard(failed);

        assert_eq!(table.count(), 1);
        assert_eq!(table.find(0x1000).map(|pin| pin.generation), Some(0));
        assert_eq!(table.find(0x2000), None);
    }

    #[test]
    fn commit_clears_the_generation_of_the_kept_pins() {
        let mut table = PinTable::<4>::new();
        let generation = table.begin();
        table
            .insert(Pin {
                generation,
                ..page(0x1000, 0x20_0000)
            })
            .expect("pin fits");
        table.commit(generation);
        assert_eq!(table.find(0x1000).map(|pin| pin.generation), Some(0));
    }

    #[test]
    fn a_full_table_refuses_a_pin() {
        let mut table = PinTable::<1>::new();
        table.insert(page(0x1000, 0x20_0000)).expect("pin fits");
        assert_eq!(table.insert(page(0x2000, 0x30_0000)), Err(PinError::Full));
    }

    #[test]
    fn rejects_an_empty_or_wrapping_range() {
        let mut table = PinTable::<2>::new();
        assert_eq!(
            table.insert(Pin {
                length: 0,
                ..page(0x1000, 0x20_0000)
            }),
            Err(PinError::InvalidRange)
        );
        assert_eq!(
            table.insert(Pin {
                base: u64::MAX - PAGE_SIZE + 1,
                ..page(0x1000, 0x20_0000)
            }),
            Err(PinError::InvalidRange)
        );
    }
}
