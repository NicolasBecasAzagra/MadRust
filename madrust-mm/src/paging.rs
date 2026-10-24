// SPDX-License-Identifier: GPL-2.0-only
//! Four-level paging layout: entry fields, index extraction and span sizes.
//!
//! The boot trampoline fills the tables in assembly, because it runs before the
//! kernel can execute Rust code at its virtual address. This module holds the
//! same arithmetic as pure functions so that the index and entry computation is
//! tested on the host, and it gives the memory manager the sizes it needs to
//! check the tables against the memory map.

/// Size of a 4 KiB page.
pub const PAGE_SIZE: u64 = 4096;

/// Size of a 2 MiB large page.
pub const LARGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

/// Number of entries in every page table.
pub const ENTRIES: usize = 512;

/// Physical span of one page directory: [`ENTRIES`] large pages, that is 1 GiB.
pub const DIRECTORY_SPAN: u64 = LARGE_PAGE_SIZE * ENTRIES as u64;

/// Entry flag: the entry is valid.
pub const PRESENT: u64 = 1 << 0;

/// Entry flag: the mapping is writable.
pub const WRITABLE: u64 = 1 << 1;

/// Entry flag: the mapping is reachable from ring 3.
pub const USER: u64 = 1 << 2;

/// Entry flag: a page directory entry maps a 2 MiB page instead of a table.
pub const LARGE_PAGE: u64 = 1 << 7;

/// Bits of an entry that hold the physical address it points at.
pub const ADDRESS_MASK: u64 = 0x000f_ffff_ffff_f000;

/// Position of `address` in each level of the four-level hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Indices {
    /// Index of the entry in the PML4.
    pub pml4: usize,
    /// Index of the entry in the page directory pointer table.
    pub pdpt: usize,
    /// Index of the entry in the page directory.
    pub directory: usize,
    /// Index of the entry in the page table.
    pub table: usize,
}

impl Indices {
    /// Extract the index of `address` at every level.
    pub const fn of(address: u64) -> Self {
        Self {
            pml4: ((address >> 39) & 0x1ff) as usize,
            pdpt: ((address >> 30) & 0x1ff) as usize,
            directory: ((address >> 21) & 0x1ff) as usize,
            table: ((address >> 12) & 0x1ff) as usize,
        }
    }
}

/// Entry that points at the next level with a 4 KiB granularity.
pub const fn entry(physical: u64) -> u64 {
    (physical & ADDRESS_MASK) | PRESENT | WRITABLE
}

/// Entry that points at the next level and is reachable from ring 3.
pub const fn user_table_entry(physical: u64) -> u64 {
    entry(physical) | USER
}

/// Leaf entry that maps one 4 KiB page for ring 3.
pub const fn user_page_entry(physical: u64) -> u64 {
    entry(physical) | USER
}

/// Entry of a page directory that maps one 2 MiB page.
///
/// Returns `None` when `physical` is not 2 MiB aligned, because a large page
/// entry ignores the low 21 bits of the address.
pub const fn large_page_entry(physical: u64) -> Option<u64> {
    if physical & (LARGE_PAGE_SIZE - 1) != 0 {
        return None;
    }
    Some(entry(physical) | LARGE_PAGE)
}

/// Entry of a page directory that maps one 2 MiB page for ring 3.
///
/// `writable` clears the writable bit for a read only user range. Returns
/// `None` when `physical` is not 2 MiB aligned.
pub const fn user_large_page_entry(physical: u64, writable: bool) -> Option<u64> {
    match large_page_entry(physical) {
        None => None,
        Some(entry) => {
            let entry = entry | USER;
            if writable {
                Some(entry)
            } else {
                Some(entry & !WRITABLE)
            }
        }
    }
}

/// Number of 2 MiB pages needed to cover `[start, end)`.
pub const fn large_pages_for(start: u64, end: u64) -> u64 {
    if end <= start {
        return 0;
    }
    let size = end - start;
    let whole = size / LARGE_PAGE_SIZE;
    if size.is_multiple_of(LARGE_PAGE_SIZE) {
        whole
    } else {
        whole + 1
    }
}

/// Number of page directories needed to cover `size` bytes.
pub const fn directories_for(size: u64) -> u64 {
    let whole = size / DIRECTORY_SPAN;
    if size.is_multiple_of(DIRECTORY_SPAN) {
        whole
    } else {
        whole + 1
    }
}
