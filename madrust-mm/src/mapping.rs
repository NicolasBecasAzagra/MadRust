// SPDX-License-Identifier: GPL-2.0-only
//! User mapping records and the fixed-size table that holds them.
//!
//! The kernel runs the kernel half and the user half from one set of page
//! tables, so a page fault has to be classified against the ranges the program
//! asked for. This module keeps that classification pure: the record shape, the
//! lookup and the range edits never allocate and never touch hardware, so the
//! host unit tests cover the logic the kernel runs on the fault path.
//!
//! The kernel owns one [`MappingTable`] behind its own lock. The table is a
//! fixed array of optional records, which keeps the kernel state static and
//! makes a full table a recoverable error instead of an allocation.

/// One past the last address of the user half.
///
/// It matches `USER_SPACE_END` in `madrust-elf`, repeated here so this module
/// stays independent of the ELF loader and the address-space constant.
pub const USER_HALF_END: u64 = 0x0000_8000_0000_0000;

/// Protection bit that allows a write. It matches the Linux `PROT_WRITE`.
pub const PROT_WRITE: u64 = 1 << 1;

/// What backs a user mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingKind {
    /// Private anonymous memory, faulted in one page at a time.
    Anonymous,
    /// Read only memory borrowed from a file of the initramfs.
    ///
    /// `physical` is the physical address of the first byte of the file data
    /// and `offset` is the file offset the mapping starts at. Both are needed
    /// because the archive lives in reserved physical memory and a fault maps
    /// the page that already holds the bytes.
    File { physical: u64, offset: u64 },
}

/// One reserved range of the user half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
    /// First address of the range.
    pub base: u64,
    /// Length in bytes, always a multiple of the page size.
    pub length: u64,
    /// Protection bits, the Linux `PROT_*` value the program asked for.
    pub protection: u64,
    /// What backs the range.
    pub kind: MappingKind,
    /// `true` when the range asked for 2 MiB pages with `madvise`.
    ///
    /// It is a hint: the fault handler still falls back to a 4 KiB page when
    /// the address is not aligned or when the block cannot be allocated.
    pub huge: bool,
}

impl Mapping {
    /// Build an anonymous mapping record.
    pub const fn anonymous(base: u64, length: u64, protection: u64) -> Self {
        Self {
            base,
            length,
            protection,
            kind: MappingKind::Anonymous,
            huge: false,
        }
    }

    /// Build a read only file mapping record.
    pub const fn file(base: u64, length: u64, protection: u64, physical: u64, offset: u64) -> Self {
        Self {
            base,
            length,
            protection,
            kind: MappingKind::File { physical, offset },
            huge: false,
        }
    }

    /// One past the last address of the range.
    pub const fn end(&self) -> u64 {
        self.base.saturating_add(self.length)
    }

    /// `true` when `address` falls inside the range.
    pub fn contains(&self, address: u64) -> bool {
        address >= self.base && address < self.end()
    }

    /// `true` when a write to the range is allowed.
    pub const fn is_writable(&self) -> bool {
        self.protection & PROT_WRITE != 0
    }

    /// `true` when the range is backed by a file.
    pub const fn is_file(&self) -> bool {
        matches!(self.kind, MappingKind::File { .. })
    }

    /// Physical address that backs `address` of a file mapping.
    ///
    /// Returns `None` for an anonymous mapping and for an address outside the
    /// record. The caller checks that the result is page aligned before it
    /// installs it in a page table.
    pub fn file_physical(&self, address: u64) -> Option<u64> {
        let MappingKind::File { physical, offset } = self.kind else {
            return None;
        };
        if !self.contains(address) {
            return None;
        }
        physical
            .checked_add(offset)?
            .checked_add(address.checked_sub(self.base)?)
    }
}

/// Why the table refused an edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappingError {
    /// The table has no free slot for a new record.
    Full,
    /// The range is empty or wraps around the address space.
    InvalidRange,
    /// A write protection was requested for a file backed range.
    ReadOnly,
}

/// Fixed-size table of user mappings.
///
/// The table keeps its records in insertion order and never reorders them, so a
/// lookup scans the slots. With tens of mappings the scan costs less than any
/// index would, and the fault path stays allocation free.
#[derive(Clone, Copy)]
pub struct MappingTable<const CAPACITY: usize> {
    slots: [Option<Mapping>; CAPACITY],
}

impl<const CAPACITY: usize> Default for MappingTable<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> MappingTable<CAPACITY> {
    /// An empty table.
    pub const fn new() -> Self {
        Self {
            slots: [None; CAPACITY],
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

    /// Record that contains `address`, if any.
    ///
    /// Addresses in the kernel half never match, even when a corrupt record
    /// would cover them: the fault handler must never install a user frame over
    /// a kernel address.
    pub fn find(&self, address: u64) -> Option<Mapping> {
        if address >= USER_HALF_END {
            return None;
        }
        self.slots
            .iter()
            .flatten()
            .copied()
            .find(|mapping| mapping.contains(address))
    }

    /// `true` when any record overlaps `[base, base + length)`.
    pub fn overlaps(&self, base: u64, length: u64) -> bool {
        let end = base.saturating_add(length);
        self.slots
            .iter()
            .flatten()
            .any(|mapping| mapping.length != 0 && mapping.base < end && base < mapping.end())
    }

    /// Add a record.
    ///
    /// The caller is responsible for the range not overlapping an existing
    /// record: `mmap` removes the old records before it adds the new one.
    pub fn insert(&mut self, mapping: Mapping) -> Result<(), MappingError> {
        if mapping.length == 0 {
            return Err(MappingError::InvalidRange);
        }
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
            return Err(MappingError::Full);
        };
        *slot = Some(mapping);
        Ok(())
    }

    /// Remove every record of `[base, end)`.
    ///
    /// A record that covers the range on both sides is split in two, and a
    /// record that covers one side is trimmed. A table without a free slot for
    /// the split refuses the edit before it changes anything.
    pub fn remove_range(&mut self, base: u64, end: u64) -> Result<(), MappingError> {
        if base >= end {
            return Err(MappingError::InvalidRange);
        }
        let splits = self
            .slots
            .iter()
            .flatten()
            .filter(|mapping| mapping.base < base && end < mapping.end())
            .count();
        let free = self.slots.iter().filter(|slot| slot.is_none()).count();
        if splits > free {
            return Err(MappingError::Full);
        }

        for index in 0..CAPACITY {
            let Some(mapping) = self.slots[index] else {
                continue;
            };
            let mapping_end = mapping.end();
            if mapping_end <= base || mapping.base >= end {
                continue;
            }
            if mapping.base >= base && mapping_end <= end {
                self.slots[index] = None;
                continue;
            }
            if mapping.base < base && mapping_end > end {
                let left = Mapping {
                    length: base - mapping.base,
                    ..mapping
                };
                let right = Mapping {
                    base: end,
                    length: mapping_end - end,
                    ..mapping
                };
                self.slots[index] = Some(left);
                let Some(free_index) = self.slots.iter().position(|slot| slot.is_none()) else {
                    // The capacity check above guarantees a slot; keeping the
                    // record whole is the only non-panicking fallback.
                    self.slots[index] = Some(mapping);
                    return Err(MappingError::Full);
                };
                self.slots[free_index] = Some(right);
            } else if mapping.base < base {
                self.slots[index] = Some(Mapping {
                    length: base - mapping.base,
                    ..mapping
                });
            } else {
                self.slots[index] = Some(Mapping {
                    base: end,
                    length: mapping_end - end,
                    ..mapping
                });
            }
        }
        Ok(())
    }

    /// Set the protection of every record of `[base, end)`.
    ///
    /// Records that straddle a boundary are split there, so the requested range
    /// carries the new protection exactly and its neighbours keep theirs.
    pub fn protect(&mut self, base: u64, end: u64, protection: u64) -> Result<(), MappingError> {
        if base >= end {
            return Err(MappingError::InvalidRange);
        }
        // A file mapping borrows reserved memory and cannot take a write
        // protection, so refuse the edit before it changes any record.
        if protection & PROT_WRITE != 0 {
            let writes_file = self
                .slots
                .iter()
                .flatten()
                .any(|mapping| mapping.is_file() && mapping.base < end && base < mapping.end());
            if writes_file {
                return Err(MappingError::ReadOnly);
            }
        }
        self.split_at(base)?;
        self.split_at(end)?;
        for slot in &mut self.slots {
            if let Some(mapping) = *slot {
                if mapping.base >= base && mapping.end() <= end {
                    *slot = Some(Mapping {
                        protection,
                        ..mapping
                    });
                }
            }
        }
        Ok(())
    }

    /// Set or clear the large page hint of every record of `[base, end)`.
    ///
    /// The range is split at both boundaries, so the hint covers exactly the
    /// records the program advised. The hint does not change the addresses.
    pub fn advise(&mut self, base: u64, end: u64, huge: bool) -> Result<(), MappingError> {
        if base >= end {
            return Err(MappingError::InvalidRange);
        }
        self.split_at(base)?;
        self.split_at(end)?;
        for slot in &mut self.slots {
            if let Some(mapping) = *slot {
                if mapping.base >= base && mapping.end() <= end {
                    *slot = Some(Mapping { huge, ..mapping });
                }
            }
        }
        Ok(())
    }

    /// Drop every record.
    pub fn clear(&mut self) {
        self.slots = [None; CAPACITY];
    }

    /// Split any record that strictly contains `point` into two records.
    fn split_at(&mut self, point: u64) -> Result<(), MappingError> {
        for index in 0..CAPACITY {
            let Some(mapping) = self.slots[index] else {
                continue;
            };
            if mapping.base >= point || point >= mapping.end() {
                continue;
            }
            let Some(free_slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
                return Err(MappingError::Full);
            };
            let right = Mapping {
                base: point,
                length: mapping.end() - point,
                ..mapping
            };
            *free_slot = Some(right);
            self.slots[index] = Some(Mapping {
                length: point - mapping.base,
                ..mapping
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Mapping, MappingError, MappingTable, PROT_WRITE, USER_HALF_END};

    /// One page.
    const PAGE: u64 = 0x1000;

    fn table() -> MappingTable<8> {
        let mut table = MappingTable::new();
        table
            .insert(Mapping::anonymous(0x1000, 2 * PAGE, 0x3))
            .expect("first mapping fits");
        table
            .insert(Mapping::anonymous(0x5000, PAGE, 0x1))
            .expect("second mapping fits");
        table
    }

    #[test]
    fn finds_an_address_inside_a_mapping() {
        let table = table();
        assert_eq!(
            table.find(0x1000).map(|mapping| mapping.length),
            Some(2 * PAGE)
        );
        assert_eq!(table.find(0x2fff).map(|mapping| mapping.base), Some(0x1000));
    }

    #[test]
    fn rejects_an_address_outside_every_mapping() {
        let table = table();
        assert_eq!(table.find(0x9000), None);
    }

    #[test]
    fn rejects_an_address_in_the_gap_between_two_mappings() {
        let table = table();
        assert_eq!(table.find(0x3000), None);
        assert_eq!(table.find(0x4fff), None);
    }

    #[test]
    fn rejects_an_address_in_the_kernel_half() {
        let mut table = MappingTable::<4>::new();
        // A corrupt record that covers the kernel half must not make the
        // lookup succeed.
        table
            .insert(Mapping::anonymous(USER_HALF_END, PAGE, 0x3))
            .expect("record fits");
        assert_eq!(table.find(USER_HALF_END + 0x10), None);
        assert_eq!(table.find(0xffff_8880_0000_0000), None);
    }

    #[test]
    fn remove_clears_a_covered_mapping() {
        let mut table = table();
        table.remove_range(0x1000, 0x3000).expect("range fits");
        assert_eq!(table.count(), 1);
        assert_eq!(table.find(0x1000), None);
        assert_eq!(table.find(0x5000).map(|mapping| mapping.length), Some(PAGE));
    }

    #[test]
    fn remove_splits_a_mapping_in_the_middle() {
        let mut table = MappingTable::<8>::new();
        table
            .insert(Mapping::anonymous(0x1000, 4 * PAGE, 0x3))
            .expect("record fits");
        table.remove_range(0x2000, 0x3000).expect("range fits");
        assert_eq!(table.count(), 2);
        assert_eq!(table.find(0x1000).map(|mapping| mapping.length), Some(PAGE));
        assert_eq!(
            table.find(0x3000).map(|mapping| mapping.length),
            Some(2 * PAGE)
        );
        assert_eq!(table.find(0x2000), None);
    }

    #[test]
    fn remove_trims_a_mapping_at_one_end() {
        let mut table = table();
        table.remove_range(0x1000, 0x1800).expect("range fits");
        assert_eq!(table.find(0x1000), None);
        assert_eq!(
            table.find(0x1800).map(|mapping| mapping.length),
            Some(PAGE + 0x800)
        );
    }

    #[test]
    fn a_full_table_refuses_a_new_mapping() {
        let mut table = MappingTable::<1>::new();
        table
            .insert(Mapping::anonymous(0x1000, PAGE, 0x3))
            .expect("the only slot fits");
        assert_eq!(
            table.insert(Mapping::anonymous(0x2000, PAGE, 0x3)),
            Err(MappingError::Full)
        );
    }

    #[test]
    fn protect_splits_at_both_boundaries() {
        let mut table = MappingTable::<8>::new();
        table
            .insert(Mapping::anonymous(0x1000, 4 * PAGE, 0x3))
            .expect("record fits");
        table.protect(2 * PAGE, 4 * PAGE, 0x1).expect("range fits");
        assert_eq!(table.count(), 3);
        assert_eq!(
            table.find(0x1000).map(|mapping| mapping.protection),
            Some(0x3)
        );
        assert_eq!(
            table.find(0x2000).map(|mapping| mapping.protection),
            Some(0x1)
        );
        assert_eq!(
            table.find(0x3000).map(|mapping| mapping.protection),
            Some(0x1)
        );
        assert_eq!(
            table.find(0x4000).map(|mapping| mapping.protection),
            Some(0x3)
        );
        assert_eq!(table.find(0x1000).map(|mapping| mapping.length), Some(PAGE));
        assert_eq!(
            table.find(0x2000).map(|mapping| mapping.length),
            Some(2 * PAGE)
        );
        assert_eq!(table.find(0x4000).map(|mapping| mapping.length), Some(PAGE));
    }

    #[test]
    fn a_file_mapping_resolves_the_physical_address_of_each_page() {
        let mapping = Mapping::file(0x4000, 2 * PAGE, 0x1, 0x20_0000, 0x3000);
        assert!(mapping.is_file());
        assert!(!mapping.is_writable());
        assert_eq!(mapping.file_physical(0x4000), Some(0x20_3000));
        assert_eq!(mapping.file_physical(0x5000), Some(0x20_4000));
        assert_eq!(mapping.file_physical(0x6000), None);
        assert_eq!(
            Mapping::anonymous(0x4000, PAGE, 0x1).file_physical(0x4000),
            None
        );
    }

    #[test]
    fn advise_marks_the_range_and_survives_a_split() {
        let mut table = MappingTable::<8>::new();
        table
            .insert(Mapping::anonymous(0x1000, 4 * PAGE, 0x3))
            .expect("record fits");
        table.advise(0x2000, 0x4000, true).expect("range fits");
        assert_eq!(table.find(0x1000).map(|mapping| mapping.huge), Some(false));
        assert_eq!(table.find(0x2000).map(|mapping| mapping.huge), Some(true));
        assert_eq!(table.find(0x3000).map(|mapping| mapping.huge), Some(true));
        assert_eq!(table.find(0x4000).map(|mapping| mapping.huge), Some(false));

        table.advise(0x3000, 0x4000, false).expect("range fits");
        assert_eq!(table.find(0x2000).map(|mapping| mapping.huge), Some(true));
        assert_eq!(table.find(0x3000).map(|mapping| mapping.huge), Some(false));
    }

    #[test]
    fn protect_refuses_to_make_a_file_mapping_writable() {
        let mut table = MappingTable::<4>::new();
        table
            .insert(Mapping::file(0x1000, PAGE, 0x1, 0x20_0000, 0))
            .expect("record fits");
        assert_eq!(
            table.protect(0x1000, 0x2000, PROT_WRITE),
            Err(MappingError::ReadOnly)
        );
        assert_eq!(
            table.find(0x1000).map(|mapping| mapping.protection),
            Some(0x1)
        );
        table
            .protect(0x1000, 0x2000, 0x1)
            .expect("read stays valid");
    }

    #[test]
    fn overlaps_reports_records_that_touch_the_range() {
        let table = table();
        assert!(table.overlaps(0x2000, PAGE));
        assert!(table.overlaps(0x5000, PAGE));
        assert!(!table.overlaps(0x3000, PAGE));
        assert!(!table.overlaps(0x7000, PAGE));
    }
}
