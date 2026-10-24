// SPDX-License-Identifier: GPL-2.0-only
//! User virtual memory: the program break and the anonymous mappings.
//!
//! The kernel owns the whole user half of the address space and there is one
//! user context, so the mapping bookkeeping is a pair of statics: the program
//! break grows the image upwards, and the anonymous mappings come from a bump
//! allocator that starts at 256 MiB.
//!
//! Anonymous mappings reserve address space, they do not allocate it. `mmap`
//! records the range in [`super::mappings`] and creates not-present page table
//! entries with [`super::paging::reserve_range`]; the page fault handler in
//! [`handle_page_fault`] installs a zeroed frame the first time the program
//! touches a page. `munmap` and a `MAP_FIXED` replacement walk the page table
//! of the range and return the frames that were actually faulted in, so a
//! program that maps and unmaps in a loop does not leak physical memory.
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
//! | `0x0000_0000_1000_0000` | anonymous mappings |
//! | `0x0000_0000_8000_0000` | top of the user stack; it grows downwards |
//!
//! The running program never sees an overlap: the loader maps the image, the
//! stack builder maps the top of the user half, and the two regions in between
//! are handed out by this module.

use core::sync::atomic::{AtomicU64, Ordering};

use madrust_elf::USER_SPACE_END;
use madrust_mm::mapping::PROT_WRITE;
use madrust_mm::paging::PAGE_SIZE;

use super::errno::{EEXIST, EINVAL, ENODEV, ENOMEM};
use super::mappings;
use super::paging;
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
        unmap_region(new_end, old_end - new_end);
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

/// `mmap`: reserve an anonymous mapping and return its base address.
///
/// The call records the range, its protection and its kind, and creates the
/// page table entries as not present. It does not take a frame: the frames
/// arrive when the program touches the pages and the fault handler maps them.
///
/// File backed mappings are rejected with `ENODEV`, because this kernel has no
/// filesystem to read them from. Everything a static standard library program
/// maps is anonymous.
pub fn mmap(
    hint: u64,
    length: u64,
    protection: u64,
    flags: u64,
    descriptor: i64,
    offset: u64,
) -> Result<u64, i64> {
    if length == 0 || offset != 0 {
        return Err(EINVAL);
    }
    if flags & MAP_ANONYMOUS == 0 {
        return Err(ENODEV);
    }
    let length = checked_length(length)?;
    let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;

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

    if fixed {
        // MAP_FIXED replaces whatever the program had mapped there: replace
        // the records first, then return the frames they faulted in.
        mappings::replace(base, length, protection).map_err(|_| ENOMEM)?;
        unmap_region(base, length);
    } else {
        mappings::record(base, length, protection).map_err(|_| ENOMEM)?;
    }
    if paging::reserve_range(base, length).is_none() {
        let _ = mappings::remove(base, length);
        return Err(ENOMEM);
    }

    // A mapping is never file backed here, so the descriptor is unused. It is
    // part of the Linux signature and the program passes -1.
    let _ = descriptor;
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
    mappings::remove(address, length).map_err(|_| ENOMEM)?;
    unmap_region(address, length);
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
    mappings::protect(address, page_up(length), protection).map_err(|_| ENOMEM)?;

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

/// `madvise`: accept the advice and keep the pages as they are.
///
/// This kernel never reclaims a user mapping behind the program's back, so the
/// advice has no work to do. Rejecting an unknown advice would break a program
/// that only asks for an optimisation.
pub fn madvise(address: u64, length: u64) -> Result<(), i64> {
    if address == 0 || !address.is_multiple_of(PAGE_SIZE) {
        return Err(EINVAL);
    }
    if length == 0 {
        return Ok(());
    }
    if address
        .checked_add(length)
        .is_none_or(|end| end > USER_SPACE_END)
    {
        return Err(EINVAL);
    }
    Ok(())
}

/// Fault one page of an anonymous mapping in.
///
/// Called from the page fault handler with interrupts disabled, so the mapping
/// table lookup and the frame allocation cannot race a system call. The frame
/// comes from the buddy allocator zeroed and is mapped with the protection the
/// record carries; the faulting instruction then runs again and succeeds.
pub fn handle_page_fault(address: u64, error: u64) -> PageFaultOutcome {
    if address >= USER_SPACE_END {
        return PageFaultOutcome::KernelAddress;
    }
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
    let mut page = address;
    while page < address + length {
        let frame = memory::with(|manager| manager.alloc_zeroed_frame()).flatten();
        let Some(frame) = frame else {
            unmap_region(address, page - address);
            return Err(ENOMEM);
        };
        if paging::map_page(page, frame, true).is_none() {
            let _ = memory::with(|manager| manager.free_frame(frame));
            unmap_region(address, page - address);
            return Err(ENOMEM);
        }
        page += PAGE_SIZE;
    }
    Ok(())
}

/// Remove every present page of the range and return its frames to the
/// allocator.
///
/// Pages that were never faulted have no entry, so this walks rows of 512 page
/// table entries and touches only the leaves that the range covers.
fn unmap_region(address: u64, length: u64) {
    let mut page = address;
    while page < address + length {
        if let Some(frame) = paging::unmap_page(page) {
            let _ = memory::with(|manager| manager.free_frame(frame));
        }
        page += PAGE_SIZE;
    }
}
