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
//! ## Resident ranges
//!
//! `mlock` faults the pages of a range in and records a pin in
//! [`super::pins`]. A pinned page is never written to swap, which is trivial
//! without a swap file, and, more importantly, [`unmap_region`] never returns
//! an owned frame that a pin still holds to the allocator. The frames stay with
//! the pin until `munlock`; a page that is still mapped when the pin ends stays
//! with its mapping, exactly like an ordinary page. A range that was faulted as
//! a 2 MiB block is pinned at [`LARGE_PAGE_ORDER`], so the last page of a
//! partial unlock gives the whole block back at that order.
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
use madrust_mm::mapping::{Mapping, PROT_WRITE};
use madrust_mm::paging::{LARGE_PAGE_SIZE, PAGE_SIZE};
use madrust_mm::pin::Pin;
use madrust_mm::{phys_to_virt, LARGE_PAGE_ORDER};

use super::errno::{EEXIST, EINVAL, ENODEV, ENOMEM, EROFS};
use super::{fd, mappings, paging, pins};
use crate::memory;

/// Where a page table walk learns whether a present page is file backed.
///
/// It matters because the frames of a file page belong to the initramfs
/// archive and must not go back to the allocator.
#[derive(Clone, Copy)]
enum Backing {
    /// No record covers the range, so every present page is anonymous. The
    /// program break and the eager mapping path use this.
    Anonymous,
    /// Ask the mapping table, which still holds the records of the range.
    Records,
}

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
pub const PAGE_FAULT_WRITE: u64 = 1 << 1;

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
        if let Err(error) = map_region_eager(old_end, new_end - old_end) {
            // The program asked for more memory and the kernel could not
            // give it: the reason is in the log, because the program only
            // sees that an allocation failed.
            crate::kprintln!(
                "heap: the break could not grow to {new_end:#x} ({} bytes): {error:?}",
                new_end - old_end
            );
            return floor;
        }
    } else if new_end < old_end {
        // The break has no mapping record, so every page it holds is
        // anonymous and the walk frees them all.
        unmap_region(new_end, old_end - new_end, Backing::Anonymous);
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
        // MAP_FIXED replaces whatever the program had mapped there. The page
        // table walk runs first, while the records still name the old kind of
        // each page, because a file page is unmapped without freeing the
        // archive memory behind it.
        unmap_region(base, length, Backing::Records);
        mappings::replace(base, length, mapping).map_err(|_| ENOMEM)?;
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
    // The walk runs before the table forgets the range: it has to know whether
    // a present page belongs to a file mapping, whose frames stay with the
    // archive instead of going back to the allocator.
    unmap_region(address, length, Backing::Records);
    mappings::remove(address, length).map_err(|_| ENOMEM)?;
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

/// `mlock`: make a mapped range resident and pin its frames.
///
/// The call faults every page of the range in, then records a pin per page or
/// per 2 MiB block in [`super::pins`]. A pin is all or nothing: a failed call
/// removes the pins it added and reports [`ENOMEM`], so the pages it managed to
/// fault in stay mapped and ordinary instead of staying half pinned.
///
/// The address must be page aligned, like Linux, and the length is rounded up
/// to whole pages. A range that is not mapped and has no present page cannot be
/// made resident, so it reports [`ENOMEM`].
pub fn mlock(address: u64, length: u64) -> Result<(), i64> {
    let Some(end) = lock_range(address, length)? else {
        return Ok(());
    };
    let generation = pins::begin();
    let mut cursor = address;
    while cursor < end {
        let Some(candidate) = lock_unit(cursor, end) else {
            pins::discard(generation);
            return Err(ENOMEM);
        };
        if pins::covers(cursor, candidate.size) {
            // A pin that outlived a `munmap` still owns its frames, but the
            // address is no longer mapped: the program cannot ask for it to be
            // resident again until it maps the range again.
            if paging::frame_at(cursor).is_none() {
                pins::discard(generation);
                return Err(ENOMEM);
            }
            cursor += candidate.size;
            continue;
        }
        // A unit that only partly overlaps an existing pin is taken page by
        // page: a pin is page aligned, so the pages are then either covered or
        // free and two records never cover the same page.
        let unit = if candidate.size > PAGE_SIZE && pins::overlaps(cursor, candidate.size) {
            let Some(page) = lock_page_unit(cursor) else {
                pins::discard(generation);
                return Err(ENOMEM);
            };
            page
        } else {
            candidate
        };
        if pins::covers(cursor, unit.size) {
            if paging::frame_at(cursor).is_none() {
                pins::discard(generation);
                return Err(ENOMEM);
            }
            cursor += unit.size;
            continue;
        }
        if pin_unit(cursor, &unit, generation).is_err() {
            pins::discard(generation);
            return Err(ENOMEM);
        }
        cursor += unit.size;
    }
    pins::commit(generation);
    Ok(())
}

/// `munlock`: release the pin of a range and give back what it no longer holds.
///
/// A partial unlock trims the pin, so the pages that stay inside the remaining
/// range keep the promise. An owned allocation goes back to the allocator only
/// when no pin holds it any more and the page table no longer maps it: a page
/// that is still mapped stays with its mapping and is freed by `munmap`.
pub fn munlock(address: u64, length: u64) -> Result<(), i64> {
    let Some(end) = lock_range(address, length)? else {
        return Ok(());
    };
    loop {
        let pin = match pins::claim(address, end) {
            Ok(Some(pin)) => pin,
            Ok(None) => return Ok(()),
            Err(_) => return Err(ENOMEM),
        };
        if pin.owned
            && !pins::holds_physical(pin.physical, pin.allocation_size())
            && !pin_is_mapped(&pin)
        {
            release_pin(&pin);
        }
    }
}

/// Validate an `mlock` or `munlock` range and return its end.
///
/// Returns `Ok(None)` for an empty range, which both calls accept as a no-op.
/// The address must be page aligned and the rounded length must stay inside the
/// user half, like Linux.
fn lock_range(address: u64, length: u64) -> Result<Option<u64>, i64> {
    if address == 0 || !address.is_multiple_of(PAGE_SIZE) {
        return Err(EINVAL);
    }
    if length == 0 {
        return Ok(None);
    }
    let length = page_up_checked(length).ok_or(ENOMEM)?;
    let end = address.checked_add(length).ok_or(ENOMEM)?;
    if end > USER_SPACE_END {
        return Err(ENOMEM);
    }
    Ok(Some(end))
}

/// `length` rounded up to a page boundary, or `None` when it overflows.
fn page_up_checked(length: u64) -> Option<u64> {
    length
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
}

/// One piece of a locked range that maps to a single allocation.
struct LockUnit {
    /// Size of the piece in bytes.
    size: u64,
    /// Where the physical memory of the piece comes from.
    kind: LockKind,
}

/// How a [`LockUnit`] is backed.
enum LockKind {
    /// One 4 KiB frame.
    Page,
    /// A 2 MiB block the pin faults in now.
    FreshBlock,
    /// A page of an existing 2 MiB leaf; the value is the block base.
    LargeLeaf(u64),
}

/// Classify the piece of a locked range that starts at `address`.
///
/// Returns `None` when `address` is neither inside a mapping nor present, which
/// is the unmapped case `mlock` reports as [`ENOMEM`]. The unit is a 2 MiB block
/// when the whole block fits in the requested range and either a large leaf
/// already maps it or the mapping asked for large pages and the page directory
/// entry is still free. Every other address is a 4 KiB pin, including a page of
/// an existing large leaf when the request starts or ends inside the block.
fn lock_unit(address: u64, end: u64) -> Option<LockUnit> {
    if let Some(block) = paging::large_page_at(address) {
        let chunk = address & !(LARGE_PAGE_SIZE - 1);
        let size = if address == chunk && chunk + LARGE_PAGE_SIZE <= end {
            LARGE_PAGE_SIZE
        } else {
            PAGE_SIZE
        };
        return Some(LockUnit {
            size,
            kind: LockKind::LargeLeaf(block),
        });
    }
    let Some(mapping) = mappings::find(address) else {
        return paging::frame_at(address).map(|_| LockUnit {
            size: PAGE_SIZE,
            kind: LockKind::Page,
        });
    };
    let chunk = address & !(LARGE_PAGE_SIZE - 1);
    if mapping.huge
        && address == chunk
        && chunk + LARGE_PAGE_SIZE <= mapping.end()
        && chunk + LARGE_PAGE_SIZE <= end
        && paging::large_page_slot_free(chunk)
    {
        return Some(LockUnit {
            size: LARGE_PAGE_SIZE,
            kind: LockKind::FreshBlock,
        });
    }
    Some(LockUnit {
        size: PAGE_SIZE,
        kind: LockKind::Page,
    })
}

/// Classify `address` as one 4 KiB page.
///
/// Used when a larger unit only partly overlaps an existing pin. The page keeps
/// the block of an existing large leaf, so the pin still gives the block back
/// at the large order when its last page is unlocked.
fn lock_page_unit(address: u64) -> Option<LockUnit> {
    if let Some(block) = paging::large_page_at(address) {
        return Some(LockUnit {
            size: PAGE_SIZE,
            kind: LockKind::LargeLeaf(block),
        });
    }
    if paging::frame_at(address).is_some() || mappings::find(address).is_some() {
        return Some(LockUnit {
            size: PAGE_SIZE,
            kind: LockKind::Page,
        });
    }
    None
}

/// Fault one unit in and record its pin under `generation`.
fn pin_unit(address: u64, unit: &LockUnit, generation: u32) -> Result<(), i64> {
    let pin = match unit.kind {
        LockKind::Page => {
            if paging::frame_at(address).is_none()
                && handle_page_fault(address, 0) != PageFaultOutcome::Resolved
            {
                return Err(ENOMEM);
            }
            // A fault at a 2 MiB boundary can install a block even for a page
            // sized unit. The pin then owns that block, so it must keep the
            // order the allocator used and give the whole block back.
            let (physical, order) = match paging::large_page_at(address) {
                Some(block) => (block, LARGE_PAGE_ORDER),
                None => {
                    let Some(frame) = paging::frame_at(address) else {
                        return Err(ENOMEM);
                    };
                    (frame, 0)
                }
            };
            let file = mappings::find(address).is_some_and(|mapping| mapping.is_file());
            Pin {
                base: address,
                length: unit.size,
                physical,
                order,
                owned: order > 0 || !file,
                generation,
            }
        }
        LockKind::FreshBlock => {
            if paging::frame_at(address).is_none()
                && handle_page_fault(address, 0) != PageFaultOutcome::Resolved
            {
                return Err(ENOMEM);
            }
            let Some(block) = paging::large_page_at(address) else {
                return Err(ENOMEM);
            };
            Pin {
                base: address,
                length: unit.size,
                physical: block,
                order: LARGE_PAGE_ORDER,
                owned: true,
                generation,
            }
        }
        LockKind::LargeLeaf(block) => Pin {
            base: address,
            length: unit.size,
            physical: block,
            order: LARGE_PAGE_ORDER,
            owned: true,
            generation,
        },
    };
    pins::insert(pin).map_err(|_| ENOMEM)
}

/// `true` when the page table still maps the allocation of `pin`.
fn pin_is_mapped(pin: &Pin) -> bool {
    if pin.order == 0 {
        paging::frame_at(pin.base) == Some(pin.physical)
    } else {
        paging::large_page_at(pin.base) == Some(pin.physical)
    }
}

/// Return the allocation of `pin` to the allocator at the order it was taken.
fn release_pin(pin: &Pin) {
    if pin.order == 0 {
        let _ = memory::with(|manager| manager.free_frame(pin.physical));
    } else {
        let _ = memory::with(|manager| manager.free_block(pin.physical, pin.order));
    }
}

/// Number of pages the program pinned.
pub fn locked_pages() -> u64 {
    pins::locked_pages()
}

/// Number of bytes the program pinned.
pub fn locked_bytes() -> u64 {
    pins::locked_bytes()
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
    let mut page = address;
    while page < address + length {
        let frame = memory::with(|manager| manager.alloc_zeroed_frame()).flatten();
        let Some(frame) = frame else {
            unmap_region(address, page - address, Backing::Anonymous);
            return Err(ENOMEM);
        };
        if paging::map_page(page, frame, true).is_none() {
            let _ = memory::with(|manager| manager.free_frame(frame));
            unmap_region(address, page - address, Backing::Anonymous);
            return Err(ENOMEM);
        }
        page += PAGE_SIZE;
    }
    Ok(())
}

/// Remove every present leaf of `[address, address + length)` and return memory
/// to the allocator at the order it was taken.
///
/// `backing` says where the walk learns whether a page is file backed. A 4 KiB
/// page that belongs to a file mapping is unmapped without a free: the archive
/// frames are reserved and were never allocated to this mapping. A 2 MiB block
/// under a file mapping is different: it is the copy the fault handler built,
/// so it goes back to the buddy allocator like an anonymous block. A frame that
/// a pin still holds is unmapped but not freed: the pin owns it until `munlock`,
/// even though the program can no longer reach it through this mapping. Pages
/// that were never faulted have no entry, so they cost a table walk and nothing
/// else.
///
/// The callers pass [`Backing::Records`] before they edit the table, so the
/// records of the range are still there when the walk asks for them.
fn unmap_region(address: u64, length: u64, backing: Backing) {
    let mut page = address;
    while page < address + length {
        let file_backed = matches!(backing, Backing::Records)
            && mappings::find(page).is_some_and(|mapping| mapping.is_file());
        match paging::unmap(page) {
            Some(paging::Unmapped::Page(frame)) if !file_backed => {
                if !pins::holds_physical(frame, PAGE_SIZE) {
                    let _ = memory::with(|manager| manager.free_frame(frame));
                }
            }
            Some(paging::Unmapped::LargePage(block)) => {
                if !pins::holds_physical(block, LARGE_PAGE_SIZE) {
                    let _ = memory::with(|manager| manager.free_block(block, LARGE_PAGE_ORDER));
                }
            }
            Some(_) | None => {}
        }
        page += PAGE_SIZE;
    }
}
