// SPDX-License-Identifier: GPL-2.0-only
//! Physical memory management for MadRust (milestone M2a).
//!
//! [`buddy`] manages 4 KiB physical frames and [`heap`] carves frames into a
//! size-class kernel heap. Neither module touches hardware or allocates memory
//! of its own, so the algorithms run unchanged in host unit tests.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod buddy;
pub mod heap;
pub mod store;

pub use buddy::BuddyAllocator;
pub use heap::{HeapError, SizeClassHeap};
pub use store::{BlockStore, StaticStore};

#[cfg(test)]
mod tests;

/// Size of one physical frame in bytes.
pub const FRAME_SIZE: usize = 4096;

/// Largest buddy order the allocator handles.
///
/// Order 0 is one frame; order 15 is `1 << 15` frames, that is 128 MiB.
pub const MAX_ORDER: u8 = 15;

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
