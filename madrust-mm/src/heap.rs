// SPDX-License-Identifier: GPL-2.0-only
//! Size-class kernel heap on top of the physical frame allocator.
//!
//! Requests up to [`MAX_SMALL`] bytes are served from the size classes in
//! [`SIZE_CLASSES`]; larger requests take `2^n` whole frames. The free lists
//! and the block descriptors live inside this structure, never in the memory
//! the heap hands out, because the allocator cannot allocate its own
//! bookkeeping.
//!
//! Each class owns up to [`MAX_SLABS`] one-frame slabs carved into blocks of
//! the class size. When the last live block of a class is freed, its slabs go
//! back to the frame allocator, so an idle heap holds no frames at all.
//!
//! A frame index is not a virtual address: the kernel runs from the higher
//! half, so every frame is turned into a pointer with [`crate::phys_to_virt`]
//! and every large block is turned back into an index with
//! [`crate::virt_to_phys`]. Frame 0 is the low reserved memory that holds the
//! real-mode structures, so the frame source must never hand it out; should it
//! happen anyway, the heap gives the frame back and reports a failed allocation
//! instead of handing out that page.

use core::ptr::NonNull;

use crate::{phys_to_virt, virt_to_phys, FrameError, FrameSource, FRAME_SIZE, MAX_ORDER};

/// Block sizes served by the small allocator.
pub const SIZE_CLASSES: [usize; 7] = [16, 32, 64, 128, 256, 512, 1024];

/// Largest request served from a size class.
pub const MAX_SMALL: usize = SIZE_CLASSES[SIZE_CLASSES.len() - 1];

/// Minimum alignment of every returned pointer.
pub const MIN_ALIGN: usize = 16;

/// Number of size classes.
pub const CLASS_COUNT: usize = SIZE_CLASSES.len();

/// Number of one-frame slabs a single class can own.
pub const MAX_SLABS: usize = 4;

/// Sentinel for "no node" and "no slab".
const NO_NODE: u16 = u16::MAX;

/// Blocks carved from one slab of each class.
const BLOCKS_PER_SLAB: [usize; CLASS_COUNT] = {
    let mut blocks = [0; CLASS_COUNT];
    let mut class = 0;
    while class < CLASS_COUNT {
        blocks[class] = FRAME_SIZE / SIZE_CLASSES[class];
        class += 1;
    }
    blocks
};

/// Block descriptors available to each class.
const NODES_PER_CLASS: [usize; CLASS_COUNT] = {
    let mut nodes = [0; CLASS_COUNT];
    let mut class = 0;
    while class < CLASS_COUNT {
        nodes[class] = MAX_SLABS * BLOCKS_PER_SLAB[class];
        class += 1;
    }
    nodes
};

/// Index of the first descriptor of each class inside the flat node array.
const NODE_BASE: [usize; CLASS_COUNT] = {
    let mut base = [0; CLASS_COUNT];
    let mut class = 1;
    while class < CLASS_COUNT {
        base[class] = base[class - 1] + NODES_PER_CLASS[class - 1];
        class += 1;
    }
    base
};

/// Total number of block descriptors.
const TOTAL_NODES: usize = NODE_BASE[CLASS_COUNT - 1] + NODES_PER_CLASS[CLASS_COUNT - 1];

/// Initial slot pool head of every class.
const SLOT_HEADS: [u16; CLASS_COUNT] = {
    let mut heads = [0; CLASS_COUNT];
    let mut class = 0;
    while class < CLASS_COUNT {
        heads[class] = NODE_BASE[class] as u16;
        class += 1;
    }
    heads
};

/// Initial number of parked descriptors of every class.
const FREE_SLOTS: [u16; CLASS_COUNT] = {
    let mut slots = [0; CLASS_COUNT];
    let mut class = 0;
    while class < CLASS_COUNT {
        slots[class] = NODES_PER_CLASS[class] as u16;
        class += 1;
    }
    slots
};

/// One descriptor of a free block.
///
/// A descriptor is either attached to a free block, in which case `addr` is the
/// block address, or parked in the slot pool of its class, in which case only
/// `next` matters.
#[derive(Clone, Copy)]
struct Node {
    addr: usize,
    next: u16,
}

/// Reason why a heap operation was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeapError {
    /// The heap is out of blocks or out of frames.
    OutOfMemory,
    /// A zero-byte request or release.
    ZeroSize,
    /// The pointer is not aligned to [`MIN_ALIGN`] or, for a large block, not
    /// aligned to a frame.
    Misaligned,
    /// The class has no live blocks, so the block was already released.
    DoubleFree,
    /// The frame allocator rejected the block.
    Frame(FrameError),
}

/// A size-class heap with static bookkeeping.
pub struct SizeClassHeap {
    /// Block descriptors, grouped per class.
    nodes: [Node; TOTAL_NODES],
    /// Head of the free block list of each class.
    free_heads: [u16; CLASS_COUNT],
    /// Head of the free descriptor list of each class.
    slot_heads: [u16; CLASS_COUNT],
    /// Number of descriptors parked in the slot pool of each class.
    free_slots: [u16; CLASS_COUNT],
    /// Head frame of every slab owned by each class.
    slab_bases: [[usize; MAX_SLABS]; CLASS_COUNT],
    /// Number of slabs owned by each class.
    slab_counts: [u8; CLASS_COUNT],
    /// Number of blocks handed out and not yet released, per class.
    live: [usize; CLASS_COUNT],
}

impl SizeClassHeap {
    /// Create an empty heap. The caller has to provide frames to grow it.
    pub const fn new() -> Self {
        let mut nodes = [Node {
            addr: 0,
            next: NO_NODE,
        }; TOTAL_NODES];
        let mut class = 0;
        while class < CLASS_COUNT {
            let base = NODE_BASE[class];
            let count = NODES_PER_CLASS[class];
            let mut index = 0;
            while index < count {
                let next = if index + 1 < count {
                    (base + index + 1) as u16
                } else {
                    NO_NODE
                };
                nodes[base + index] = Node { addr: 0, next };
                index += 1;
            }
            class += 1;
        }

        Self {
            nodes,
            free_heads: [NO_NODE; CLASS_COUNT],
            slot_heads: SLOT_HEADS,
            free_slots: FREE_SLOTS,
            slab_bases: [[0; MAX_SLABS]; CLASS_COUNT],
            slab_counts: [0; CLASS_COUNT],
            live: [0; CLASS_COUNT],
        }
    }

    /// Allocate `size` bytes.
    ///
    /// Returns a pointer aligned to at least [`MIN_ALIGN`] bytes, or `None`
    /// when the heap cannot satisfy the request.
    pub fn alloc<F: FrameSource>(&mut self, frames: &mut F, size: usize) -> Option<NonNull<u8>> {
        if size == 0 {
            return None;
        }
        let class = match class_for(size) {
            Some(class) => class,
            None => return self.alloc_large(frames, size),
        };

        if self.free_heads[class] == NO_NODE {
            self.grow(frames, class).ok()?;
        }

        let head = self.free_heads[class];
        let node = *self.nodes.get(usize::from(head))?;
        self.free_heads[class] = node.next;

        // The descriptor is free again while the block is handed out.
        let next_slot = self.slot_heads[class];
        let slot = self.nodes.get_mut(usize::from(head))?;
        slot.addr = 0;
        slot.next = next_slot;
        self.slot_heads[class] = head;
        self.free_slots[class] += 1;
        self.live[class] += 1;

        NonNull::new(node.addr as *mut u8)
    }

    /// Release a block obtained from [`SizeClassHeap::alloc`].
    ///
    /// The caller must pass the same `size` that was used to allocate the
    /// block, and must not use the block afterwards.
    pub fn dealloc<F: FrameSource>(
        &mut self,
        frames: &mut F,
        ptr: NonNull<u8>,
        size: usize,
    ) -> Result<(), HeapError> {
        if size == 0 {
            return Err(HeapError::ZeroSize);
        }
        let addr = ptr.as_ptr() as usize;
        if !addr.is_multiple_of(MIN_ALIGN) {
            return Err(HeapError::Misaligned);
        }
        let class = match class_for(size) {
            Some(class) => class,
            None => return self.dealloc_large(frames, addr, size),
        };

        // Every block of a class is released in the same order, so a release
        // with no live block left can only be a double free.
        let live = self.live[class];
        if live == 0 {
            return Err(HeapError::DoubleFree);
        }
        let slot = self.take_slot(class).ok_or(HeapError::OutOfMemory)?;
        let next = self.free_heads[class];
        let node = self
            .nodes
            .get_mut(usize::from(slot))
            .ok_or(HeapError::OutOfMemory)?;
        node.addr = addr;
        node.next = next;
        self.free_heads[class] = slot;
        self.live[class] = live - 1;

        if self.live[class] == 0 {
            self.release_class(frames, class)?;
        }
        Ok(())
    }

    /// Allocate `size` bytes as a power-of-two run of frames.
    fn alloc_large<F: FrameSource>(&mut self, frames: &mut F, size: usize) -> Option<NonNull<u8>> {
        let order = pages_order(size)?;
        let frame = frames.alloc_frames(order)?;
        if frame == 0 {
            // Frame 0 holds the low reserved memory, which is never a valid
            // block. Hand it back instead of leaking the block.
            let _ = frames.free_frames(frame, order);
            return None;
        }
        NonNull::new(frame_address(frame) as *mut u8)
    }

    /// Release a block obtained from [`SizeClassHeap::alloc_large`].
    fn dealloc_large<F: FrameSource>(
        &mut self,
        frames: &mut F,
        addr: usize,
        size: usize,
    ) -> Result<(), HeapError> {
        let physical = virt_to_phys(addr as u64).ok_or(HeapError::Misaligned)?;
        if physical % FRAME_SIZE as u64 != 0 {
            return Err(HeapError::Misaligned);
        }
        let order = pages_order(size).ok_or(HeapError::OutOfMemory)?;
        frames
            .free_frames((physical / FRAME_SIZE as u64) as usize, order)
            .map_err(HeapError::Frame)
    }

    /// Add one slab to `class`, carving it into class-sized blocks.
    fn grow<F: FrameSource>(&mut self, frames: &mut F, class: usize) -> Result<(), HeapError> {
        let slabs = usize::from(self.slab_counts[class]);
        if slabs >= MAX_SLABS {
            return Err(HeapError::OutOfMemory);
        }
        let blocks = BLOCKS_PER_SLAB[class];
        if usize::from(self.free_slots[class]) < blocks {
            return Err(HeapError::OutOfMemory);
        }

        let frame = frames.alloc_frames(0).ok_or(HeapError::OutOfMemory)?;
        let base = frame_address(frame) as usize;
        let stride = SIZE_CLASSES[class];
        for index in 0..blocks {
            let slot = self.take_slot(class).ok_or(HeapError::OutOfMemory)?;
            let next = self.free_heads[class];
            if let Some(node) = self.nodes.get_mut(usize::from(slot)) {
                node.addr = base + index * stride;
                node.next = next;
            }
            self.free_heads[class] = slot;
        }

        self.slab_bases[class][slabs] = frame;
        self.slab_counts[class] = (slabs + 1) as u8;
        Ok(())
    }

    /// Give every slab of `class` back to the frame allocator and reset it.
    fn release_class<F: FrameSource>(
        &mut self,
        frames: &mut F,
        class: usize,
    ) -> Result<(), HeapError> {
        let slabs = usize::from(self.slab_counts[class]);
        for frame in self.slab_bases[class].iter().take(slabs) {
            frames.free_frames(*frame, 0).map_err(HeapError::Frame)?;
        }
        self.slab_counts[class] = 0;
        self.free_heads[class] = NO_NODE;

        // No live block and an empty free list: every descriptor of the class
        // is parked again.
        let base = NODE_BASE[class];
        let count = NODES_PER_CLASS[class];
        for index in 0..count {
            let next = if index + 1 < count {
                (base + index + 1) as u16
            } else {
                NO_NODE
            };
            if let Some(node) = self.nodes.get_mut(base + index) {
                node.addr = 0;
                node.next = next;
            }
        }
        self.slot_heads[class] = base as u16;
        self.free_slots[class] = count as u16;
        Ok(())
    }

    /// Take a descriptor from the slot pool of `class`.
    fn take_slot(&mut self, class: usize) -> Option<u16> {
        let slot = self.slot_heads[class];
        if slot == NO_NODE {
            return None;
        }
        let next = self.nodes.get(usize::from(slot))?.next;
        self.slot_heads[class] = next;
        self.free_slots[class] -= 1;
        Some(slot)
    }
}

impl Default for SizeClassHeap {
    fn default() -> Self {
        Self::new()
    }
}

/// Virtual address of the first byte of `frame`.
///
/// The kernel runs from the higher half, so a frame index only becomes a usable
/// pointer once it goes through the direct map.
fn frame_address(frame: usize) -> u64 {
    phys_to_virt(frame as u64 * FRAME_SIZE as u64)
}

/// Size class that serves `size`, or `None` for a large request.
fn class_for(size: usize) -> Option<usize> {
    let mut class = 0;
    while class < CLASS_COUNT {
        if size <= SIZE_CLASSES[class] {
            return Some(class);
        }
        class += 1;
    }
    None
}

/// Buddy order needed for `size` bytes rounded up to whole frames.
fn pages_order(size: usize) -> Option<u8> {
    let pages = size.checked_add(FRAME_SIZE - 1)? / FRAME_SIZE;
    let rounded = pages.checked_next_power_of_two()?;
    let order = rounded.trailing_zeros() as u8;
    if order > MAX_ORDER {
        None
    } else {
        Some(order)
    }
}
