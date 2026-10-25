// SPDX-License-Identifier: GPL-2.0-only
//! Physical memory management for MadRust (milestone M2c).
//!
//! [`buddy`] manages 4 KiB physical frames and [`heap`] carves frames into a
//! size-class kernel heap. Neither module touches hardware or allocates memory
//! of its own, so the algorithms run unchanged in host unit tests.
//!
//! The kernel runs from the higher half, so a frame index is not a virtual
//! address any more: physical memory is reachable only through the direct map
//! that [`phys_to_virt`] translates into. [`paging`] holds the page table
//! layout and the entry arithmetic in a form that host tests can check.
//! [`mapping`] holds the anonymous and file backed user mapping records and
//! their lookup, also free of hardware and allocation, so the fault path is
//! testable on the host. [`pin`] holds the resident range records that `mlock`
//! creates, with the same restriction, so the pinning policy is testable too.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod buddy;
pub mod heap;
pub mod mapping;
pub mod paging;
pub mod pin;
pub mod store;

pub use buddy::BuddyAllocator;
pub use heap::{HeapError, SizeClassHeap};
pub use store::{BlockStore, SliceStore};

#[cfg(test)]
mod tests;

/// Size of one physical frame in bytes.
pub const FRAME_SIZE: usize = 4096;

/// Base of the direct map of physical memory.
///
/// Chosen in the middle of the canonical upper half, 1 GiB aligned so that one
/// page directory covers the first GiB of physical memory, and far from the
/// kernel half at `0xffffffff80000000`: the kernel image is linked in PML4
/// entry 511 and the direct map occupies PML4 entry 273.
pub const DIRECT_MAP_BASE: u64 = 0xffff_8880_0000_0000;

/// Page directories the boot code reserves for the direct map.
///
/// A page directory maps 1 GiB with 2 MiB pages, so the reservation covers
/// 32 GiB of physical memory. It matches `DIRECT_MAP_DIRECTORIES` in
/// `madrust-kernel/src/trampoline.s`; the boot code refuses a guest with more
/// RAM than that instead of leaving part of it unmapped, and the memory
/// manager rejects the same case if the boot path ever hands it over.
pub const DIRECT_MAP_DIRECTORIES: u64 = 32;

/// Physical memory the direct map covers.
pub const DIRECT_MAP_SPAN: u64 = DIRECT_MAP_DIRECTORIES * paging::DIRECTORY_SPAN;

/// Translate a physical address into its direct map virtual address.
///
/// Only physical addresses below [`DIRECT_MAP_SPAN`] are mapped; the caller
/// checks the memory map against that limit before it hands out frames.
pub const fn phys_to_virt(physical: u64) -> u64 {
    DIRECT_MAP_BASE + physical
}

/// Translate a direct map virtual address back into a physical address.
///
/// Returns `None` for an address outside the direct map: the kernel half, the
/// user half and the part of the upper half without a mapping are not physical
/// memory.
pub const fn virt_to_phys(virtual_address: u64) -> Option<u64> {
    match virtual_address.checked_sub(DIRECT_MAP_BASE) {
        Some(offset) if offset < DIRECT_MAP_SPAN => Some(offset),
        _ => None,
    }
}

/// Buddy order that covers one 2 MiB large page.
///
/// One 4 KiB frame is order 0, so 512 frames are order 9.
pub const LARGE_PAGE_ORDER: u8 = 9;

/// Largest buddy order the allocator handles.
///
/// Order 0 is one frame; order 20 is `1 << 20` frames, that is 4 GiB. The
/// order is stored in the low seven bits of one metadata byte, so the encoding
/// still fits and `OWNED`/`COVERED`/`RESERVED` stay unambiguous.
pub const MAX_ORDER: u8 = 20;

/// Reason why a frame operation was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// The metadata store cannot describe the requested frame count.
    MetadataTooSmall { required: usize, available: usize },
    /// The frame or range lies outside the managed frames.
    OutOfRange { frame: usize },
    /// The requested buddy order is above [`MAX_ORDER`].
    OrderTooLarge { requested: u8, max: u8 },
    /// No free block of the requested order exists.
    OutOfMemory,
    /// The frame is not the head of an allocated block: it is already free,
    /// part of a larger block, reserved, or was never released.
    NotAllocated { frame: usize },
    /// The frame heads an allocated block, but of a different order.
    OrderMismatch {
        frame: usize,
        stored: u8,
        requested: u8,
    },
    /// The frame was already released to the allocator.
    AlreadyManaged { frame: usize },
}

/// A source of physical frames for the kernel heap.
pub trait FrameSource {
    /// Allocate a block of `1 << order` frames and return its head frame index.
    fn alloc_frames(&mut self, order: u8) -> Option<usize>;

    /// Give back a block obtained from [`FrameSource::alloc_frames`].
    fn free_frames(&mut self, frame: usize, order: u8) -> Result<(), FrameError>;
}
