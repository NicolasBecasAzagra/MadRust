// SPDX-License-Identifier: GPL-2.0-only
//! User virtual memory: the program break, anonymous mappings and file
//! mappings over the initramfs.
//!
//! The kernel owns the whole user half of the address space and there is one
//! user context, so the mapping bookkeeping is a pair of statics: the program
//! break grows the image upwards, and the mappings come from a bump allocator
//! that starts at 256 MiB.
//!
//! Mappings reserve address space, they do not allocate it. `mmap` records the
//! range in [`super::mappings`]; the page fault handler in
//! [`handle_page_fault`] installs a zeroed frame for an anonymous page, a
//! 2 MiB block when `madvise(MADV_HUGEPAGE)` marked the range and the fault is
//! aligned, or the physical page the initramfs already holds for a file page.
//! A file page in a large page range is different: the archive bytes are not
//! aligned to 2 MiB, so the fault copies the file data into a fresh 2 MiB block
//! and maps that block read only.
//! `munmap` and a `MAP_FIXED` replacement walk the page table of the range and
//! return anonymous memory and copied file blocks to the allocator, so a
//! program that maps and unmaps in a loop does not leak physical memory. A
//! 4 KiB file page is unmapped without a free: those archive frames belong to
//! the initramfs, not to the mapping.
//!
//! The program break keeps its eager behaviour: `brk` maps and unmaps its
//! frames directly, without a record in the mapping table. `mremap` is not
//! supported and rejects with `EINVAL`; no program of this milestone needs it.
//!
//! ## Layout
//!
//! | Range | Use |
//! |-------|-----|
//! | `0x0000_0000_0040_0000` | loaded image, then the program break |
//! | `0x0000_0000_1000_0000` | mappings |
//! | `0x0000_0000_8000_0000` | top of the user stack; it grows downwards |
//!
//! The running program never sees an overlap: the loader maps the image, the
//! stack builder maps the top of the user half, and the two regions in between
//! are handed out by this module.

use core::sync::atomic::{AtomicU64, Ordering};

use madrust_elf::USER_SPACE_END;
use madrust_mm::mapping::{Mapping, MappingTable, PROT_WRITE};
use madrust_mm::paging::{LARGE_PAGE_SIZE, PAGE_SIZE};
use madrust_mm::{phys_to_virt, LARGE_PAGE_ORDER};

use super::errno::{EEXIST, EINVAL, ENODEV, ENOMEM, EROFS};
use super::mappings::CAPACITY;
use super::{fd, mappings, paging};
use crate::memory;

/// First address handed out by `mmap`.
const MMAP_BASE: u64 = 0x0000_0000_1000_0000;

/// One past the last address `mmap` may hand out.
///
/// It stays below the copied program header page at 1.5 GiB and the stack at
/// 2 GiB, so an anonymous mapping can never reach either of them.
const MMAP_LIMIT: u64 = 0x0000_0000_5000_0000;

/// Next address the bump allocator of anonymous mappings hands out.
static NEXT_MAPPING: AtomicU64 = AtomicU64::new(MMAP_BASE);

/// Current program break, set by the loader before the program starts.
static PROGRAM_BREAK: AtomicU64 = AtomicU64::new(0);

/// `MAP_FIXED`: map exactly at the requested address, replacing what is there.
pub const MAP_FIXED: u64 = 1 << 4;

/// `MAP_ANONYMOUS`: the mapping has no file behind it.
pub const MAP_ANONYMOUS: u64 = 1 << 5;

/// `MAP_FIXED_NOREPLACE`: like `MAP_FIXED`, but fail when something is mapped.
pub const MAP_FIXED_NOREPLACE: u64 = 1 << 20;

/// `MADV_HUGEPAGE`: prefer 2 MiB pages in the range.
const MADV_HUGEPAGE: u64 = 14;

/// `MADV_NOHUGEPAGE`: go back to 4 KiB pages in the range.
const MADV_NOHUGEPAGE: u64 = 15;

/// Large pages the kernel mapped for anonymous memory since boot.
static LARGE_PAGES: AtomicU64 = AtomicU64::new(0);

/// Large pages the kernel built for file mappings since boot.
///
/// Every one of them is a 2 MiB block that holds a copy of the file data.
static FILE_LARGE_PAGES: AtomicU64 = AtomicU64::new(0);

/// Page faults the kernel resolved from a file mapping since boot.
///
/// A 4 KiB file page counts once, and a copied 2 MiB block counts once.
static FILE_PAGE_FAULTS: AtomicU64 = AtomicU64::new(0);

/// Page fault error code: the fault is a protection violation, not a missing
/// translation. The hardware sets this bit when the entry is present.
const PAGE_FAULT_PROTECTION: u64 = 1 << 0;

/// Page fault error code: the faulting access was a write.
const PAGE_FAULT_WRITE: u64 = 1 << 1;

/// What the page fault handler did with an address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageFaultOutcome {
    /// A zeroed frame was installed; the instruction can be retried.
    Resolved,
    /// The address belongs to no anonymous mapping.
    Unmapped,
    /// The access violates the protection the mapping asked for.
    Protection,
    /// The frame allocator had no free frame.
    OutOfMemory,
    /// The address is in the kernel half, where a fault is a kernel bug.
    KernelAddress,
}

impl PageFaultOutcome {
    /// Human readable reason for the serial log.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Unmapped => "the address is not mapped",
            Self::Protection => "the access violates the mapping protection",
            Self::OutOfMemory => "no free frame for the fault",
            Self::KernelAddress => "the address is in the kernel half",
        }
    }
}

/// Round `address` up to a page boundary.
const fn page_up(address: u64) -> u64 {
    (address + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Remember the program break the loader handed over.
pub fn set_break(address: u64) {
    PROGRAM_BREAK.store(address, Ordering::Release);
}

/// Current program break.
pub fn break_address() -> u64 {
    PROGRAM_BREAK.load(Ordering::Acquire)
}

/// `brk`: move the end of the image data segment.
///
/// The frames are allocated eagerly, exactly as before demand paging: the
/// program break belongs to the heap, and the heap self test and the static
/// runtime rely on the pages being present as soon as the break moves.
///
/// Returns the break that is in force after the call, which is the Linux
/// contract: a program detects a rejected request by comparing the result with
/// the value it asked for.
pub fn sys_brk(requested: u64) -> u64 {
    let floor = break_address();
    if requested == 0 || requested < floor {
        return floor;
    }
    if requested == floor {
        return floor;
    }
    if requested >= MMAP_BASE {
        return floor;
    }

    let old_end = mapped_break_end(floor);
    let new_end = page_up(requested);
    if new_end > old_end {
        if map_region_eager(old_end, new_end - old_end).is_err() {
            return floor;
        }
    } else if new_end < old_end {
        // The break has no mapping record, so every page it holds is
        // anonymous and the empty table makes the walk free them all.
        unmap_region(&MappingTable::new(), new_end, old_end - new_end);
    }

    PROGRAM_BREAK.store(requested, Ordering::Release);
    requested
}

/// First address above the program break that is not mapped yet.
///
/// The page that holds the initial break belongs to the image, so a break that
/// has never moved only maps from the next page boundary on.
fn mapped_break_end(floor: u64) -> u64 {
    let current = break_address();
    if current <= floor {
        return page_up(floor);
    }
    page_up(current)
}

/// Reject a length that cannot be a mapping of this address space.
fn checked_length(length: u64) -> Result<u64, i64> {
    if length > MMAP_LIMIT {
        return Err(ENOMEM);
    }
    Ok(page_up(length))
}

/// `mmap`: reserve an anonymous or file backed mapping and return its base.
///
/// The call records the range and its protection and takes no frame. An
/// anonymous page faults in a zeroed frame the first time it is touched. A
/// file page is mapped to the physical page the initramfs already holds, so
/// the mapping copies nothing and frees nothing.
///
/// A file mapping is read only: `PROT_WRITE` reports `EROFS`, the offset must
/// be page aligned, the length covers whole pages, and a range that reaches
/// past the end of the file reports `EINVAL`.
pub fn mmap(
    hint: u64,
    length: u64,
    protection: u64,
    flags: u64,
    descriptor: i64,
    offset: u64,
) -> Result<u64, i64> {
    if length == 0 {
        return Err(EINVAL);
    }
    let length = checked_length(length)?;
    let anonymous = flags & MAP_ANONYMOUS != 0;
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;

    let file = if anonymous {
        if offset != 0 {
            return Err(EINVAL);
        }
        None
    } else {
        if !offset.is_multiple_of(PAGE_SIZE) {
            return Err(EINVAL);
        }
        if protection & PROT_WRITE != 0 {
            return Err(EROFS);
        }
        let Ok(descriptor) = u64::try_from(descriptor) else {
            return Err(ENODEV);
        };
        let (physical, size) = fd::file_backing(descriptor)?;
        let end = offset.checked_add(length).ok_or(EINVAL)?;
        if end > size {
            return Err(EINVAL);
        }
        let physical = physical.checked_add(offset).ok_or(EINVAL)?;
        if !physical.is_multiple_of(PAGE_SIZE) {
            return Err(EINVAL);
        }
        Some((physical, offset))
    };

    let base = if fixed {
        if hint == 0 || !hint.is_multiple_of(PAGE_SIZE) {
            return Err(EINVAL);
        }
        let end = hint.checked_add(length).ok_or(ENOMEM)?;
        if end > USER_SPACE_END {
            return Err(ENOMEM);
        }
        if flags & MAP_FIXED_NOREPLACE != 0 && !region_is_free(hint, length) {
            return Err(EEXIST);
        }
        hint
    } else {
        allocate_address(length)?
    };

    let mapping = match file {
        Some((physical, offset)) => Mapping::file(base, length, protection, physical, offset),
        None => Mapping::anonymous(base, length, protection),
    };

    if fixed {
        // MAP_FIXED replaces whatever the program had mapped there. The
        // snapshot keeps the old kind of each present page, so a file page is
        // unmapped without freeing the archive memory behind it.
        let previous = mappings::snapshot();
        mappings::replace(base, length, mapping).map_err(|_| ENOMEM)?;
        unmap_region(&previous, base, length);
    } else {
        mappings::record(mapping).map_err(|_| ENOMEM)?;
    }
    Ok(base)
}

/// Reserve `length` bytes from the anonymous mapping bump allocator.
fn allocate_address(length: u64) -> Result<u64, i64> {
    loop {
        let current = NEXT_MAPPING.load(Ordering::Acquire);
        let end = current.checked_add(length).ok_or(ENOMEM)?;
        if end > MMAP_LIMIT {
            return Err(ENOMEM);
        }
        if NEXT_MAPPING
            .compare_exchange(current, end, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(current);
        }
    }
}

/// `munmap`: remove a mapping and give the faulted frames back.
pub fn munmap(address: u64, length: u64) -> Result<(), i64> {
    if address == 0 || !address.is_multiple_of(PAGE_SIZE) {
        return Err(EINVAL);
    }
    if length == 0 {
        return Ok(());
    }
    let length = checked_length(length)?;
    let end = address.checked_add(length).ok_or(EINVAL)?;
    if end > USER_SPACE_END {
        return Err(EINVAL);
    }
    let previous = mappings::snapshot();
    mappings::remove(address, length).map_err(|_| ENOMEM)?;
    unmap_region(&previous, address, length);
    Ok(())
}

/// `mprotect`: change the protection of a range of mapped pages.
///
/// The record carries the protection used by the next fault, and the pages that
/// are already present are updated in place. The user half has no execute bit
/// without `EFER.NXE`, so `PROT_EXEC` and `PROT_READ` only require the page to
/// exist. `PROT_NONE` is not honoured: the page stays readable, which is what a
/// freestanding loader without execute protection can promise.
pub fn mprotect(address: u64, length: u64, protection: u64) -> Result<(), i64> {
    if address == 0 || !address.is_multiple_of(PAGE_SIZE) {
        return Err(EINVAL);
    }
    if length == 0 || length > MMAP_LIMIT {
        return Err(EINVAL);
    }
    let end = address.checked_add(page_up(length)).ok_or(ENOMEM)?;
    if end > USER_SPACE_END {
        return Err(ENOMEM);
    }

    // Ranges outside the table belong to the image or to the break; their
    // present pages still change, and a missing page needs no record.
    match mappings::protect(address, page_up(length), protection) {
        Ok(()) => {}
        Err(madrust_mm::mapping::MappingError::ReadOnly) => return Err(EROFS),
        Err(_) => return Err(ENOMEM),
    }

    let writable = protection & PROT_WRITE != 0;
    let mut page = address;
    while page < end {
        // A page that was never faulted has no entry to update; the record now
        // carries the new protection and the fault handler will use it.
        let _ = paging::set_writable(page, writable);
        page += PAGE_SIZE;
    }
    Ok(())
}

/// `mremap`: not supported.
///
/// Resizing a mapping would have to move the records and the present frames,
/// and no program of this milestone asks for it. The call reports `EINVAL`, so
/// a program sees an explicit refusal instead of a silent wrong resizing.
pub fn mremap(
    _address: u64,
    _old_length: u64,
    _new_length: u64,
    _flags: u64,
    _new_address: u64,
) -> Result<u64, i64> {
    Err(EINVAL)
}

/// `madvise`: record or clear the large page hint of a range.
///
/// The hint is not an order. A fault inside a hinted range still falls back to
/// a 4 KiB page when the aligned chunk is not fully inside the mapping, when the
/// page directory entry is already in use, or when no 2 MiB block is free. For
/// a file mapping the block is a copy of the file data. Any other advice is
/// accepted and ignored, so a program that only asks for an optimisation is not
/// broken by the refusal.
pub fn madvise(address: u64, length: u64, advice: u64) -> Result<(), i64> {
    if address == 0 || !address.is_multiple_of(PAGE_SIZE) {
        return Err(EINVAL);
    }
    if length == 0 {
        return Ok(());
    }
    let length = page_up(length);
    if address
        .checked_add(length)
        .is_none_or(|end| end > USER_SPACE_END)
    {
        return Err(EINVAL);
    }
    let huge = match advice {
        MADV_HUGEPAGE => Some(true),
        MADV_NOHUGEPAGE => Some(false),
        _ => None,
    };
    if let Some(huge) = huge {
        mappings::advise(address, length, huge).map_err(|_| ENOMEM)?;
    }
    Ok(())
}

/// Build and map one 2 MiB file page.
///
/// The initramfs data is not aligned to 2 MiB, and a large page entry can only
/// name an aligned physical block. The function therefore takes a fresh block,
/// copies the 2 MiB of file data into it and maps the block read only. The copy
/// is the whole cost of the alignment, and the block is owned by the mapping
/// from then on: [`unmap_region`] returns it to the buddy allocator.
///
/// Returns `None` when the file range, the block or the page directory cannot
/// provide the large page; the caller then installs a 4 KiB archive page.
fn map_file_large_page(mapping: &Mapping, address: u64) -> Option<()> {
    let source = mapping.file_physical(address)?;
    let block = memory::with(|manager| manager.alloc_block(LARGE_PAGE_ORDER)).flatten()?;
    // SAFETY: the mapping guarantees `source` is the first byte of 2 MiB of
    // file data inside the reserved initramfs, and `block` is a 2 MiB block the
    // allocator just removed from its free lists. Both are translated by the
    // direct map, the two ranges do not overlap, and this is the only reference
    // to the block until it is installed in the page table.
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys_to_virt(source) as *const u8,
            phys_to_virt(block) as *mut u8,
            LARGE_PAGE_SIZE as usize,
        );
    }
    if paging::map_large_page(address, block, false).is_some() {
        return Some(());
    }
    let _ = memory::with(|manager| manager.free_block(block, LARGE_PAGE_ORDER));
    None
}

/// Fault one page in.
///
/// Called from the page fault handler with interrupts disabled, so the mapping
/// table lookup and the frame allocation cannot race a system call.
///
/// A file page is mapped to the physical page the archive already holds and
/// takes no frame. An anonymous page takes a zeroed frame, or a zeroed 2 MiB
/// block when the range asked for large pages and the fault lands on a 2 MiB
/// boundary. The faulting instruction then runs again and succeeds.
pub fn handle_page_fault(address: u64, error: u64) -> PageFaultOutcome {
    if address >= USER_SPACE_END {
        return PageFaultOutcome::KernelAddress;
    }
    // CR2 holds the address of the access, not the page. Every page table
    // operation below works on the page base, so an access in the middle of a
    // page does not turn the file's physical page into an unaligned address.
    let address = address & !(PAGE_SIZE - 1);
    // A protection violation means the entry is present: the page exists, the
    // access does not fit it. Never replace such an entry with a new frame.
    if error & PAGE_FAULT_PROTECTION != 0 {
        return PageFaultOutcome::Protection;
    }
    let Some(mapping) = mappings::find(address) else {
        return PageFaultOutcome::Unmapped;
    };
    if error & PAGE_FAULT_WRITE != 0 && !mapping.is_writable() {
        return PageFaultOutcome::Protection;
    }
    if mapping.is_file() {
        // A file page is served at the aligned 2 MiB chunk that contains the
        // fault, so the first touch of the chunk maps all of it. The chunk must
        // lie fully inside the mapping: a mapping that starts or ends in the
        // middle of a chunk keeps those edge pages on 4 KiB.
        let chunk = address & !(LARGE_PAGE_SIZE - 1);
        if mapping.huge
            && chunk >= mapping.base
            && chunk + LARGE_PAGE_SIZE <= mapping.end()
            && paging::large_page_slot_free(chunk)
            && map_file_large_page(&mapping, chunk).is_some()
        {
            FILE_LARGE_PAGES.fetch_add(1, Ordering::Relaxed);
            FILE_PAGE_FAULTS.fetch_add(1, Ordering::Relaxed);
            return PageFaultOutcome::Resolved;
        }
        let Some(physical) = mapping.file_physical(address) else {
            return PageFaultOutcome::Protection;
        };
        if paging::map_page(address, physical, false).is_none() {
            return PageFaultOutcome::OutOfMemory;
        }
        FILE_PAGE_FAULTS.fetch_add(1, Ordering::Relaxed);
        madrust_arch::cpu::invalidate_page(address);
        return PageFaultOutcome::Resolved;
    }
    if mapping.huge
        && address.is_multiple_of(LARGE_PAGE_SIZE)
        && address + LARGE_PAGE_SIZE <= mapping.end()
    {
        let block = memory::with(|manager| manager.alloc_zeroed_block(LARGE_PAGE_ORDER)).flatten();
        if let Some(block) = block {
            if paging::map_large_page(address, block, mapping.is_writable()).is_some() {
                LARGE_PAGES.fetch_add(1, Ordering::Relaxed);
                madrust_arch::cpu::invalidate_page(address);
                return PageFaultOutcome::Resolved;
            }
            let _ = memory::with(|manager| manager.free_block(block, LARGE_PAGE_ORDER));
        }
    }
    let Some(frame) = memory::with(|manager| manager.alloc_zeroed_frame()).flatten() else {
        return PageFaultOutcome::OutOfMemory;
    };
    if paging::map_page(address, frame, mapping.is_writable()).is_none() {
        let _ = memory::with(|manager| manager.free_frame(frame));
        return PageFaultOutcome::OutOfMemory;
    }
    madrust_arch::cpu::invalidate_page(address);
    PageFaultOutcome::Resolved
}

/// Number of 2 MiB pages the kernel mapped for anonymous memory since boot.
pub fn large_pages() -> u64 {
    LARGE_PAGES.load(Ordering::Relaxed)
}

/// Number of 2 MiB pages the kernel built for file mappings since boot.
pub fn file_large_pages() -> u64 {
    FILE_LARGE_PAGES.load(Ordering::Relaxed)
}

/// Number of page faults the kernel resolved from a file mapping since boot.
pub fn file_page_faults() -> u64 {
    FILE_PAGE_FAULTS.load(Ordering::Relaxed)
}

/// Number of mapping slots in use and in the table.
pub fn mapping_usage() -> (usize, usize) {
    mappings::usage()
}

/// `true` when no page of `[address, address + length)` is mapped.
fn region_is_free(address: u64, length: u64) -> bool {
    if mappings::overlaps(address, length) {
        return false;
    }
    let mut page = address;
    while page < address + length {
        if paging::frame_at(page).is_some() {
            return false;
        }
        page += PAGE_SIZE;
    }
    true
}

/// Allocate and map zeroed frames for every page of the range.
///
/// Used by the program break, which keeps the eager behaviour. A failure rolls
/// the whole range back, so a failed mapping never leaves a partial range
/// behind.
fn map_region_eager(address: u64, length: u64) -> Result<(), i64> {
    let empty = MappingTable::new();
    let mut page = address;
    while page < address + length {
        let frame = memory::with(|manager| manager.alloc_zeroed_frame()).flatten();
        let Some(frame) = frame else {
            unmap_region(&empty, address, page - address);
            return Err(ENOMEM);
        };
        if paging::map_page(page, frame, true).is_none() {
            let _ = memory::with(|manager| manager.free_frame(frame));
            unmap_region(&empty, address, page - address);
            return Err(ENOMEM);
        }
        page += PAGE_SIZE;
    }
    Ok(())
}

/// Remove every present leaf of the range and return memory to the allocator
/// at the order it was taken.
///
/// `previous` is the mapping table before the edit. A 4 KiB page that belonged
/// to a file mapping is unmapped without a free: the archive frames are
/// reserved and were never allocated to this mapping. A 2 MiB block under a
/// file mapping is different: it is the copy the fault handler built, so it
/// goes back to the buddy allocator like an anonymous block. Pages that were
/// never faulted have no entry, so they cost a table walk and nothing else.
fn unmap_region(previous: &MappingTable<CAPACITY>, address: u64, length: u64) {
    let mut page = address;
    while page < address + length {
        let file_backed = previous.find(page).is_some_and(|mapping| mapping.is_file());
        match paging::unmap(page) {
            Some(paging::Unmapped::Page(frame)) if !file_backed => {
                let _ = memory::with(|manager| manager.free_frame(frame));
            }
            Some(paging::Unmapped::LargePage(block)) => {
                let _ = memory::with(|manager| manager.free_block(block, LARGE_PAGE_ORDER));
            }
            Some(_) | None => {}
        }
        page += PAGE_SIZE;
    }
}
