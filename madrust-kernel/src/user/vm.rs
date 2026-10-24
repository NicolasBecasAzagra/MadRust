// SPDX-License-Identifier: GPL-2.0-only
//! User virtual memory: the program break and the anonymous mappings.
//!
//! The kernel owns the whole user half of the address space and there is one
//! user context, so the mapping bookkeeping is a pair of statics: the program
//! break grows the image upwards, and the anonymous mappings come from a bump
//! allocator that starts at 256 MiB.
//!
//! Every mapping uses 4 KiB pages taken from the frame allocator and installed
//! in the user half with [`super::paging::map_page`]. Unmapping returns the
//! frames to the allocator, so a program that maps and unmaps in a loop does
//! not leak physical memory. Only the virtual address space is not recycled:
//! `munmap` and `mremap` release the frames, and the next `mmap` continues from
//! the bump pointer.
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
use madrust_mm::paging::PAGE_SIZE;

use super::errno::{EEXIST, EINVAL, ENODEV, ENOMEM};
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

/// `PROT_WRITE`: the mapping may be written.
pub const PROT_WRITE: u64 = 1 << 1;

/// `MAP_FIXED`: map exactly at the requested address, replacing what is there.
pub const MAP_FIXED: u64 = 1 << 4;

/// `MAP_ANONYMOUS`: the mapping has no file behind it.
pub const MAP_ANONYMOUS: u64 = 1 << 5;

/// `MAP_FIXED_NOREPLACE`: like `MAP_FIXED`, but fail when something is mapped.
pub const MAP_FIXED_NOREPLACE: u64 = 1 << 20;

/// `MREMAP_MAYMOVE`: the kernel may move the mapping to satisfy the size.
pub const MREMAP_MAYMOVE: u64 = 1;

/// `MREMAP_FIXED`: move the mapping to the requested address.
pub const MREMAP_FIXED: u64 = 2;

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
        if map_region(old_end, new_end - old_end).is_err() {
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

/// `mmap`: create an anonymous mapping and return its base address.
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
        if flags & MAP_FIXED_NOREPLACE != 0 {
            if !region_is_free(hint, length) {
                return Err(EEXIST);
            }
        } else {
            // MAP_FIXED replaces whatever the program had mapped there.
            unmap_region(hint, length);
        }
        hint
    } else {
        allocate_address(length)?
    };

    map_region(base, length)?;
    if protection & PROT_WRITE == 0 {
        for page in 0..length / PAGE_SIZE {
            paging::set_writable(base + page * PAGE_SIZE, false);
        }
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

/// `munmap`: remove a mapping and give its frames back.
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
    unmap_region(address, length);
    Ok(())
}

/// `mprotect`: change the write permission of a range of mapped pages.
///
/// The user half has no execute bit without `EFER.NXE`, so `PROT_EXEC` and
/// `PROT_READ` only require the page to exist. `PROT_NONE` is not honoured:
/// the page stays readable, which is what a freestanding loader without
/// execute protection can promise.
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

    let writable = protection & PROT_WRITE != 0;
    let mut page = address;
    while page < end {
        if !paging::set_writable(page, writable) {
            return Err(ENOMEM);
        }
        page += PAGE_SIZE;
    }
    Ok(())
}

/// `mremap`: resize an existing anonymous mapping.
///
/// A shrinking request unmaps the tail. A growing request extends the mapping
/// in place when the pages above it are free, and otherwise allocates a new
/// range, copies the bytes and removes the old one when `MREMAP_MAYMOVE` is
/// set.
pub fn mremap(
    address: u64,
    old_length: u64,
    new_length: u64,
    flags: u64,
    new_address: u64,
) -> Result<u64, i64> {
    if address == 0 || !address.is_multiple_of(PAGE_SIZE) {
        return Err(EINVAL);
    }
    if old_length == 0 || new_length == 0 {
        return Err(EINVAL);
    }

    let old_length = checked_length(old_length)?;
    let new_length = checked_length(new_length)?;

    if new_length == old_length {
        return Ok(address);
    }
    if new_length < old_length {
        unmap_region(address + new_length, old_length - new_length);
        return Ok(address);
    }

    let tail = address.checked_add(old_length).ok_or(ENOMEM)?;
    if tail
        .checked_add(new_length - old_length)
        .filter(|end| *end <= USER_SPACE_END)
        .is_some_and(|_| region_is_free(tail, new_length - old_length))
        && map_region(tail, new_length - old_length).is_ok()
    {
        return Ok(address);
    }

    if flags & MREMAP_MAYMOVE == 0 {
        return Err(ENOMEM);
    }

    let base = if flags & MREMAP_FIXED != 0 {
        if new_address == 0 || !new_address.is_multiple_of(PAGE_SIZE) {
            return Err(EINVAL);
        }
        let new_end = new_address.checked_add(new_length).ok_or(ENOMEM)?;
        // A fixed destination that overlaps the source would unmap the bytes
        // being copied, so the request is invalid.
        if new_address < address + old_length && address < new_end {
            return Err(EINVAL);
        }
        unmap_region(new_address, new_length);
        new_address
    } else {
        allocate_address(new_length)?
    };
    map_region(base, new_length)?;

    // SAFETY: both ranges are mapped in the live page tables of the running
    // program, the source range was readable and the destination range was
    // just allocated, and they do not overlap: the new mapping was either
    // disjoint from the old one or the old range was free. A user program
    // cannot run while the kernel copies, because interrupts only reach the
    // kernel paths.
    unsafe {
        core::ptr::copy_nonoverlapping(
            address as *const u8,
            base as *mut u8,
            usize::try_from(old_length.min(new_length)).unwrap_or(0),
        );
    }
    unmap_region(address, old_length);
    Ok(base)
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

/// `true` when no page of `[address, address + length)` is mapped.
fn region_is_free(address: u64, length: u64) -> bool {
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
/// A failure rolls the whole range back, so a failed mapping never leaves a
/// partial range behind.
fn map_region(address: u64, length: u64) -> Result<(), i64> {
    let mut page = address;
    while page < address + length {
        let frame = memory::with(|manager| manager.alloc_zeroed_frame()).flatten();
        let Some(frame) = frame else {
            unmap_region(address, page - address);
            return Err(ENOMEM);
        };
        if paging::map_page(page, frame).is_none() {
            let _ = memory::with(|manager| manager.free_frame(frame));
            unmap_region(address, page - address);
            return Err(ENOMEM);
        }
        page += PAGE_SIZE;
    }
    Ok(())
}

/// Remove every mapping of the range and return the frames to the allocator.
fn unmap_region(address: u64, length: u64) {
    let mut page = address;
    while page < address + length {
        if let Some(frame) = paging::unmap_page(page) {
            let _ = memory::with(|manager| manager.free_frame(frame));
        }
        page += PAGE_SIZE;
    }
}
