// SPDX-License-Identifier: GPL-2.0-only
//! User half page tables.
//!
//! The user program shares the kernel page tables: this module fills PML4
//! entry 0, which the boot code cleared when it removed the identity map, and
//! leaves the kernel and direct map entries untouched. Every level of a user
//! mapping carries the user bit, so ring 3 can reach the leaf page while the
//! kernel half stays supervisor only.
//!
//! Table pages come from the frame allocator and are written through the
//! direct map. There is one address space for the whole milestone: no PML4 is
//! cloned and no address space is destroyed when the program exits.
//!
//! [`map_page`] fills a 4 KiB leaf when the page fault handler takes the
//! frame, and [`map_large_page`] installs a 2 MiB leaf in the page directory
//! when the range was marked with `madvise(MADV_HUGEPAGE)`. The tables are
//! created on demand, so an `mmap` that is never touched takes no frame.

use core::ptr::addr_of;

use madrust_mm::paging;
use madrust_mm::{phys_to_virt, FRAME_SIZE};

use crate::memory;

extern "C" {
    /// Top level page table built by the boot trampoline.
    static mm_pml4: u8;
}

/// Physical address an entry points at.
fn entry_address(entry: u64) -> u64 {
    entry & paging::ADDRESS_MASK
}

/// Pointer to a page table page through the direct map.
fn table_pointer(physical: u64) -> *mut u64 {
    phys_to_virt(physical) as *mut u64
}

/// Allocate a zeroed page table page and return its physical address.
fn allocate_table() -> Option<u64> {
    let frame = memory::with(|manager| manager.alloc_frame())??;

    // SAFETY: `frame` is a 4 KiB frame the allocator just removed from its
    // free lists, the direct map translates it, and no other reference to it
    // exists, so the zeroing store is exclusive.
    unsafe {
        core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, FRAME_SIZE);
    }
    Some(frame)
}

/// Read one entry of a page table page.
fn read_entry(table: *mut u64, index: usize) -> u64 {
    // SAFETY: `table` points at a page table page owned by the kernel and
    // `index` is a valid entry index. The read is volatile because the CPU
    // updates the accessed and dirty bits in the same word.
    unsafe { table.add(index).read_volatile() }
}

/// Write one present entry of a page table page.
fn write_entry(table: *mut u64, index: usize, value: u64) {
    // SAFETY: `table` points at a page table page owned by the kernel and
    // `index` is a valid entry index; no other code writes this entry.
    unsafe { table.add(index).write_volatile(value) };
}

/// Follow, or create, one level of the page table walk.
///
/// A present 2 MiB leaf is not a table: the caller handles it before it reaches
/// this function, so a large page entry reports `None` instead of turning the
/// mapped physical block into a table.
fn next_table(table: *mut u64, index: usize) -> Option<*mut u64> {
    let entry = read_entry(table, index);
    if entry & paging::PRESENT != 0 {
        if entry & paging::LARGE_PAGE != 0 {
            return None;
        }
        return Some(table_pointer(entry_address(entry)));
    }
    let fresh = allocate_table()?;
    write_entry(table, index, paging::user_table_entry(fresh));
    Some(table_pointer(fresh))
}

/// Pointer to the page directory entry that maps the 2 MiB span of `address`.
fn directory_entry(address: u64) -> Option<*mut u64> {
    let indices = paging::Indices::of(address);
    // SAFETY: `mm_pml4` is the static page table page the kernel loads in CR3,
    // and every entry followed below was present in the previous level.
    let mut table = addr_of!(mm_pml4) as *mut u64;
    for index in [indices.pml4, indices.pdpt] {
        let entry = read_entry(table, index);
        if entry & paging::PRESENT == 0 {
            return None;
        }
        table = table_pointer(entry_address(entry));
    }
    Some(
        // SAFETY: `table` points at the page directory for this address and
        // `indices.directory` is a valid entry index within it.
        unsafe { table.add(indices.directory) },
    )
}

/// Pointer to the leaf entry that maps `address`, if every level exists.
fn leaf_entry(address: u64) -> Option<*mut u64> {
    let indices = paging::Indices::of(address);
    let directory = directory_entry(address)?;
    let entry = read_entry(directory, 0);
    if entry & paging::PRESENT == 0 || entry & paging::LARGE_PAGE != 0 {
        return None;
    }
    let table = table_pointer(entry_address(entry));
    Some(
        // SAFETY: `table` points at the page table page for this address and
        // `indices.table` is a valid entry index within it.
        unsafe { table.add(indices.table) },
    )
}

/// Physical address backing `address`, if the user half maps it.
///
/// A 2 MiB leaf covers the whole span, so every address inside it resolves to
/// the matching offset of the physical block.
pub fn frame_at(address: u64) -> Option<u64> {
    if let Some(directory) = directory_entry(address) {
        let entry = read_entry(directory, 0);
        if entry & paging::PRESENT != 0 && entry & paging::LARGE_PAGE != 0 {
            let offset = address & (paging::LARGE_PAGE_SIZE - 1);
            return Some(entry_address(entry) + offset);
        }
    }
    let entry = read_entry(leaf_entry(address)?, 0);
    if entry & paging::PRESENT == 0 {
        return None;
    }
    Some(entry_address(entry))
}

/// One leaf removed from the page tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unmapped {
    /// One 4 KiB frame.
    Page(u64),
    /// One 2 MiB block, allocated at [`madrust_mm::LARGE_PAGE_ORDER`].
    LargePage(u64),
}

/// Remove one user leaf and return the physical memory it held.
///
/// Returns `None` when the address is not mapped. The caller owns the result
/// and returns it to the allocator at the matching order.
pub fn unmap(address: u64) -> Option<Unmapped> {
    if let Some(directory) = directory_entry(address) {
        let entry = read_entry(directory, 0);
        if entry & paging::PRESENT != 0 && entry & paging::LARGE_PAGE != 0 {
            let base = address & !(paging::LARGE_PAGE_SIZE - 1);
            write_entry(directory, 0, 0);
            madrust_arch::cpu::invalidate_page(base);
            return Some(Unmapped::LargePage(entry_address(entry)));
        }
    }
    let table = leaf_entry(address)?;
    let entry = read_entry(table, 0);
    if entry & paging::PRESENT == 0 {
        return None;
    }
    write_entry(table, 0, 0);
    madrust_arch::cpu::invalidate_page(address);
    Some(Unmapped::Page(entry_address(entry)))
}

/// Make a mapped user page writable or read only.
///
/// Returns `false` when the address is not mapped. Every user page of this
/// milestone is present and reachable from ring 3; only the writable bit and
/// the executable state change here, and x86-64 has no executable bit without
/// `EFER.NXE`, so a non executable page stays readable and executable.
pub fn set_writable(address: u64, writable: bool) -> bool {
    if let Some(directory) = directory_entry(address) {
        let entry = read_entry(directory, 0);
        if entry & paging::PRESENT != 0 && entry & paging::LARGE_PAGE != 0 {
            let updated = if writable {
                entry | paging::WRITABLE
            } else {
                entry & !paging::WRITABLE
            };
            write_entry(directory, 0, updated);
            madrust_arch::cpu::invalidate_page(address & !(paging::LARGE_PAGE_SIZE - 1));
            return true;
        }
    }
    let Some(table) = leaf_entry(address) else {
        return false;
    };
    let entry = read_entry(table, 0);
    if entry & paging::PRESENT == 0 {
        return false;
    }
    let updated = if writable {
        entry | paging::WRITABLE
    } else {
        entry & !paging::WRITABLE
    };
    write_entry(table, 0, updated);
    madrust_arch::cpu::invalidate_page(address);
    true
}

/// Map one 4 KiB user page at `address`.
///
/// Intermediate tables are created on demand. Returns `None` when a frame for
/// a table page cannot be allocated or when `physical` is not page aligned.
pub fn map_page(address: u64, physical: u64, writable: bool) -> Option<()> {
    if !physical.is_multiple_of(paging::PAGE_SIZE) {
        return None;
    }
    let indices = paging::Indices::of(address);
    // SAFETY: `mm_pml4` is the static page table page the kernel loads in CR3.
    let mut table = addr_of!(mm_pml4) as *mut u64;
    for index in [indices.pml4, indices.pdpt, indices.directory] {
        table = next_table(table, index)?;
    }
    let entry = if writable {
        paging::user_page_entry(physical)
    } else {
        paging::user_page_entry(physical) & !paging::WRITABLE
    };
    write_entry(table, indices.table, entry);
    Some(())
}

/// `true` when a 2 MiB leaf can still be installed at `address`.
///
/// The page directory entry must be empty: a present entry is either an
/// existing 2 MiB leaf or a page table that holds 4 KiB leaves, and neither can
/// be replaced by a large page without losing mappings.
pub fn large_page_slot_free(address: u64) -> bool {
    match directory_entry(address) {
        Some(directory) => read_entry(directory, 0) & paging::PRESENT == 0,
        None => true,
    }
}

/// Map one 2 MiB user page at `address`.
///
/// The page directory entry must still be empty, so a range that already
/// faulted 4 KiB pages is left alone and the caller falls back to the small
/// page path. Returns `None` when the address or the physical block is not
/// 2 MiB aligned, or when the tables cannot be created.
pub fn map_large_page(address: u64, physical: u64, writable: bool) -> Option<()> {
    if !address.is_multiple_of(paging::LARGE_PAGE_SIZE)
        || !physical.is_multiple_of(paging::LARGE_PAGE_SIZE)
    {
        return None;
    }
    let indices = paging::Indices::of(address);
    // SAFETY: `mm_pml4` is the static page table page the kernel loads in CR3.
    let mut table = addr_of!(mm_pml4) as *mut u64;
    for index in [indices.pml4, indices.pdpt] {
        table = next_table(table, index)?;
    }
    if read_entry(table, indices.directory) & paging::PRESENT != 0 {
        return None;
    }
    let entry = paging::user_large_page_entry(physical, writable)?;
    write_entry(table, indices.directory, entry);
    madrust_arch::cpu::invalidate_page(address);
    Some(())
}

/// Map `pages` fresh frames ending just below `top` and return the first
/// address of the range.
///
/// Used for the user stack: the pages are contiguous in the user half, not in
/// physical memory.
pub fn map_range(top: u64, pages: u64) -> Option<u64> {
    let base = top.checked_sub(pages.checked_mul(paging::PAGE_SIZE)?)?;
    for page in 0..pages {
        let address = base + page * paging::PAGE_SIZE;
        let frame = memory::with(|manager| manager.alloc_frame())??;
        // SAFETY: the frame just came out of the allocator and nothing else
        // references it, so the zero fill is exclusive.
        unsafe {
            core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, FRAME_SIZE);
        }
        map_page(address, frame, true)?;
    }
    Some(base)
}
