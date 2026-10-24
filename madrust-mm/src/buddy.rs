// SPDX-License-Identifier: GPL-2.0-only
//! Buddy allocator over 4 KiB physical frames.
//!
//! The allocator manages a contiguous range of frame indices `0..managed`.
//! Frame index `i` describes the physical frame at `i * FRAME_SIZE`, so the
//! kernel's frame indices are physical frame numbers.
//!
//! One metadata byte per frame records the state of that frame:
//!
//! - `0x00..=0x0f`: head of a free block, the low bits hold its order;
//! - `0x80..=0x8f`: head of an allocated block, the low bits hold its order;
//! - `0xfe`: the frame is covered by a larger block and is not a head;
//! - `0xff`: the frame was never released to the allocator.
//!
//! Storing the order of allocated blocks is what makes a double free and a
//! free with the wrong order detectable instead of corrupting the free lists.

use crate::store::BlockStore;
use crate::{FrameError, FrameSource, MAX_ORDER};

/// Metadata byte of a free block head; the order is stored in the low bits.
const FREE: u8 = 0x00;
/// Metadata byte of an allocated block head; the order is stored in the low
/// bits and the high bit is always set.
const OWNED: u8 = 0x80;
/// Frame covered by a larger block.
const COVERED: u8 = 0xFE;
/// Frame that was never released to the allocator.
const RESERVED: u8 = 0xFF;
/// Mask that extracts the order from a metadata byte.
const ORDER_MASK: u8 = 0x7F;

/// Number of buddy orders, including order 0.
const ORDERS: usize = MAX_ORDER as usize + 1;

/// A buddy allocator over a contiguous range of 4 KiB frames.
#[derive(Debug)]
pub struct BuddyAllocator<S> {
    store: S,
    managed_frames: usize,
    free_frames: usize,
    free_per_order: [usize; ORDERS],
    /// Lower bound for the next search at each order. It only ever moves
    /// forward while allocating and is pulled back when a block is freed, so a
    /// search rarely rescans the whole metadata.
    hint: [usize; ORDERS],
}

impl<S: BlockStore> BuddyAllocator<S> {
    /// Create an allocator that manages `managed_frames` frames.
    ///
    /// Every frame starts reserved: [`BuddyAllocator::release_range`] hands
    /// ranges of usable RAM to the allocator. The store must be able to
    /// describe every managed frame, so a store shorter than `managed_frames`
    /// is rejected instead of silently ignoring the tail.
    pub fn new(mut store: S, managed_frames: usize) -> Result<Self, FrameError> {
        if managed_frames > store.len() {
            return Err(FrameError::MetadataTooSmall {
                required: managed_frames,
                available: store.len(),
            });
        }

        for index in 0..managed_frames {
            store.set(index, RESERVED);
        }

        Ok(Self {
            store,
            managed_frames,
            free_frames: 0,
            free_per_order: [0; ORDERS],
            hint: [0; ORDERS],
        })
    }

    /// Number of frames the allocator manages.
    pub fn managed_frames(&self) -> usize {
        self.managed_frames
    }

    /// Number of frames that are currently free, across all orders.
    pub fn free_frames(&self) -> usize {
        self.free_frames
    }

    /// Number of free blocks of `order`, or `None` when the order is invalid.
    pub fn free_blocks_at(&self, order: u8) -> Option<usize> {
        self.free_per_order.get(usize::from(order)).copied()
    }

    /// Number of free frames that live in blocks of `order`.
    pub fn free_frames_at(&self, order: u8) -> Option<usize> {
        let blocks = self.free_blocks_at(order)?;
        Some(blocks << order)
    }

    /// Release `count` reserved frames starting at `start` to the allocator.
    ///
    /// The range is cut into aligned power-of-two blocks so that every free
    /// block can be merged with its buddy later. Releasing a frame twice is
    /// rejected.
    pub fn release_range(&mut self, start: usize, count: usize) -> Result<(), FrameError> {
        let end = start
            .checked_add(count)
            .ok_or(FrameError::OutOfRange { frame: start })?;
        if end > self.managed_frames {
            return Err(FrameError::OutOfRange { frame: end - 1 });
        }

        for index in start..end {
            if self.store.get(index) != RESERVED {
                return Err(FrameError::AlreadyManaged { frame: index });
            }
        }

        let mut cursor = start;
        let mut remaining = count;
        while remaining > 0 {
            let order = fitting_order(cursor, remaining);
            self.mark_free(cursor, order);
            let size = 1usize << order;
            cursor += size;
            remaining -= size;
        }

        Ok(())
    }

    /// Allocate a block of `1 << order` frames and return its head frame.
    pub fn allocate(&mut self, order: u8) -> Result<usize, FrameError> {
        if order > MAX_ORDER {
            return Err(FrameError::OrderTooLarge {
                requested: order,
                max: MAX_ORDER,
            });
        }

        let mut level = usize::from(order);
        while level < ORDERS && self.free_per_order[level] == 0 {
            level += 1;
        }
        if level == ORDERS {
            return Err(FrameError::OutOfMemory);
        }

        // `free_per_order` said a block exists, so the scan must find it.
        let base = self.find_free(level as u8).ok_or(FrameError::OutOfMemory)?;

        // Split the block down to the requested order, keeping the lower half.
        let current = base;
        let mut current_order = level as u8;
        while current_order > order {
            self.free_per_order[usize::from(current_order)] -= 1;
            current_order -= 1;
            let half = 1usize << current_order;
            self.store.set(current, FREE | current_order);
            self.store.set(current + half, FREE | current_order);
            self.free_per_order[usize::from(current_order)] += 2;
            self.hint[usize::from(current_order)] =
                self.hint[usize::from(current_order)].min(current + half);
            // Frames that are not block heads keep their `COVERED` marker.
        }

        self.store.set(current, OWNED | order);
        self.free_per_order[usize::from(order)] -= 1;
        self.free_frames -= 1 << order;
        let size = 1usize << order;
        self.hint[usize::from(order)] = (current + size).min(self.managed_frames);

        Ok(current)
    }

    /// Free a block of `1 << order` frames that starts at `frame`.
    ///
    /// The block must come from [`BuddyAllocator::allocate`] with the same
    /// order. A frame that is not the head of an allocated block, a double
    /// free and a mismatched order are reported as errors.
    pub fn deallocate(&mut self, frame: usize, order: u8) -> Result<(), FrameError> {
        if order > MAX_ORDER {
            return Err(FrameError::OrderTooLarge {
                requested: order,
                max: MAX_ORDER,
            });
        }
        let size = 1usize << order;
        if frame >= self.managed_frames || frame + size > self.managed_frames {
            return Err(FrameError::OutOfRange { frame });
        }
        if !frame.is_multiple_of(size) {
            return Err(FrameError::NotAllocated { frame });
        }

        let state = self.store.get(frame);
        if state != OWNED | order {
            let stored_order = state & ORDER_MASK;
            if state & OWNED != 0 && stored_order <= MAX_ORDER {
                return Err(FrameError::OrderMismatch {
                    frame,
                    stored: stored_order,
                    requested: order,
                });
            }
            return Err(FrameError::NotAllocated { frame });
        }

        let mut head = frame;
        let mut level = order;
        self.store.set(head, FREE | level);
        self.free_per_order[usize::from(level)] += 1;
        self.free_frames += size;
        self.hint[usize::from(level)] = self.hint[usize::from(level)].min(head);

        // Merge with the buddy for as long as both halves are free.
        while level < MAX_ORDER {
            let buddy = head ^ (1usize << level);
            if buddy >= self.managed_frames || self.store.get(buddy) != FREE | level {
                break;
            }
            self.free_per_order[usize::from(level)] -= 2;
            self.store.set(head, COVERED);
            self.store.set(buddy, COVERED);
            level += 1;
            head = head.min(buddy);
            self.store.set(head, FREE | level);
            self.free_per_order[usize::from(level)] += 1;
            self.hint[usize::from(level)] = self.hint[usize::from(level)].min(head);
        }

        Ok(())
    }

    /// Find a free block of `order`, starting from the order hint.
    fn find_free(&mut self, order: u8) -> Option<usize> {
        let wanted = FREE | order;
        let size = 1usize << order;
        let start = self.hint[usize::from(order)].min(self.managed_frames);

        for index in start..self.managed_frames {
            if self.store.get(index) == wanted {
                self.hint[usize::from(order)] = (index + size).min(self.managed_frames);
                return Some(index);
            }
        }
        for index in 0..start {
            if self.store.get(index) == wanted {
                self.hint[usize::from(order)] = (index + size).min(self.managed_frames);
                return Some(index);
            }
        }

        self.hint[usize::from(order)] = 0;
        None
    }

    /// Mark the block at `base` as free and account for it.
    fn mark_free(&mut self, base: usize, order: u8) {
        let size = 1usize << order;
        self.store.set(base, FREE | order);
        for offset in 1..size {
            self.store.set(base + offset, COVERED);
        }
        self.free_per_order[usize::from(order)] += 1;
        self.free_frames += size;
        self.hint[usize::from(order)] = self.hint[usize::from(order)].min(base);
    }
}

impl<S: BlockStore> FrameSource for BuddyAllocator<S> {
    fn alloc_frames(&mut self, order: u8) -> Option<usize> {
        self.allocate(order).ok()
    }

    fn free_frames(&mut self, frame: usize, order: u8) -> Result<(), FrameError> {
        self.deallocate(frame, order)
    }
}

/// Largest order that fits `count` frames starting at an aligned `base`.
fn fitting_order(base: usize, count: usize) -> u8 {
    if count == 0 {
        return 0;
    }
    let alignment = if base == 0 {
        MAX_ORDER
    } else {
        base.trailing_zeros().min(u32::from(MAX_ORDER)) as u8
    };
    let capacity = (usize::BITS - 1 - count.leading_zeros()) as u8;
    alignment.min(capacity).min(MAX_ORDER)
}
