// SPDX-License-Identifier: GPL-2.0-only
//! Host unit tests for the buddy allocator and the size-class heap.

use crate::buddy::BuddyAllocator;
use crate::heap::{HeapError, SizeClassHeap, MAX_SMALL, SIZE_CLASSES};
use crate::store::VecStore;
use crate::{FrameError, FRAME_SIZE, MAX_ORDER};

/// Allocator of `frames` frames with every frame released to it.
fn released(frames: usize) -> BuddyAllocator<VecStore> {
    let mut allocator = BuddyAllocator::new(VecStore::new(frames), frames).expect("store is large");
    allocator
        .release_range(0, frames)
        .expect("release the whole range");
    allocator
}

#[test]
fn reports_managed_and_free_frames() {
    let allocator = released(64);
    assert_eq!(allocator.managed_frames(), 64);
    assert_eq!(allocator.free_frames(), 64);
    assert_eq!(allocator.free_frames_at(6), Some(64));
    assert_eq!(allocator.free_frames_at(0), Some(0));
}

#[test]
fn allocates_and_frees_a_single_frame() {
    let mut allocator = released(8);
    let frame = allocator.allocate(0).expect("frame available");
    assert!(frame < 8);
    assert_eq!(allocator.free_frames(), 7);
    allocator.deallocate(frame, 0).expect("frame was allocated");
    assert_eq!(allocator.free_frames(), 8);
}

#[test]
fn splitting_creates_aligned_buddies() {
    let mut allocator = released(8);
    assert_eq!(allocator.allocate(0), Ok(0));
    assert_eq!(allocator.free_blocks_at(0), Some(1));
    assert_eq!(allocator.free_blocks_at(1), Some(1));
    assert_eq!(allocator.free_blocks_at(2), Some(1));
    assert_eq!(allocator.free_blocks_at(3), Some(0));
    assert_eq!(allocator.free_frames(), 7);
}

#[test]
fn merging_buddies_rebuilds_a_larger_block() {
    let mut allocator = released(8);
    let first = allocator.allocate(0).expect("frame available");
    let second = allocator.allocate(0).expect("frame available");
    assert_eq!(first ^ 1, second);

    allocator.deallocate(first, 0).expect("frame was allocated");
    // The buddy is still allocated, so nothing can merge yet.
    assert_eq!(allocator.free_blocks_at(0), Some(1));
    assert_eq!(allocator.free_blocks_at(1), Some(1));

    allocator
        .deallocate(second, 0)
        .expect("frame was allocated");
    assert_eq!(allocator.free_blocks_at(0), Some(0));
    assert_eq!(allocator.free_blocks_at(3), Some(1));
    assert_eq!(allocator.free_frames(), 8);
}

#[test]
fn allocates_and_frees_a_run_of_frames() {
    let mut allocator = released(64);
    let base = allocator.allocate(3).expect("order 3 block available");
    assert_eq!(base % 8, 0);
    assert_eq!(allocator.free_frames(), 56);
    allocator.deallocate(base, 3).expect("block was allocated");
    assert_eq!(allocator.free_frames(), 64);
    assert_eq!(allocator.free_blocks_at(6), Some(1));
}

#[test]
fn exhaustion_returns_out_of_memory() {
    let mut allocator = released(4);
    for _ in 0..4 {
        assert!(allocator.allocate(0).is_ok());
    }
    assert_eq!(allocator.free_frames(), 0);
    assert_eq!(allocator.allocate(0), Err(FrameError::OutOfMemory));
}

#[test]
fn rejects_requests_outside_the_managed_range() {
    let mut allocator = released(8);
    assert_eq!(
        allocator.allocate(MAX_ORDER + 1),
        Err(FrameError::OrderTooLarge {
            requested: MAX_ORDER + 1,
            max: MAX_ORDER
        })
    );
    assert_eq!(
        allocator.release_range(4, 8),
        Err(FrameError::OutOfRange { frame: 11 })
    );
    assert_eq!(
        allocator.deallocate(8, 0),
        Err(FrameError::OutOfRange { frame: 8 })
    );
}

#[test]
fn detects_a_double_free() {
    let mut allocator = released(4);
    let frame = allocator.allocate(0).expect("frame available");
    allocator.deallocate(frame, 0).expect("frame was allocated");
    assert_eq!(
        allocator.deallocate(frame, 0),
        Err(FrameError::NotAllocated { frame })
    );
}

#[test]
fn detects_a_free_with_the_wrong_order() {
    let mut allocator = released(4);
    let base = allocator.allocate(1).expect("order 1 block available");
    assert_eq!(
        allocator.deallocate(base, 0),
        Err(FrameError::OrderMismatch {
            frame: base,
            stored: 1,
            requested: 0
        })
    );
    assert_eq!(allocator.free_frames(), 2);
}

#[test]
fn detects_a_free_of_a_non_head_frame() {
    let mut allocator = released(8);
    let base = allocator.allocate(2).expect("order 2 block available");
    assert_eq!(
        allocator.deallocate(base + 1, 0),
        Err(FrameError::NotAllocated { frame: base + 1 })
    );
}

#[test]
fn detects_a_repeated_release() {
    let mut allocator = released(8);
    assert_eq!(
        allocator.release_range(0, 4),
        Err(FrameError::AlreadyManaged { frame: 0 })
    );
}

#[test]
fn rejects_metadata_smaller_than_the_managed_frames() {
    let error = BuddyAllocator::new(VecStore::new(8), 16).expect_err("store is too small");
    assert_eq!(
        error,
        FrameError::MetadataTooSmall {
            required: 16,
            available: 8
        }
    );
}

#[test]
fn free_frame_count_returns_to_the_baseline() {
    let mut allocator = released(256);
    let baseline = allocator.free_frames();

    let mut held = Vec::new();
    let mut order = 0;
    while held.len() < 64 {
        match allocator.allocate(order) {
            Ok(frame) => held.push((frame, order)),
            Err(FrameError::OutOfMemory) => break,
            Err(error) => panic!("unexpected error: {error:?}"),
        }
        order = (order + 1) % 4;
    }
    assert_eq!(
        allocator.free_frames(),
        baseline - held.iter().map(|(_, o)| 1 << o).sum::<usize>()
    );

    for (frame, order) in held.into_iter().rev() {
        allocator
            .deallocate(frame, order)
            .expect("block was allocated");
    }
    assert_eq!(allocator.free_frames(), baseline);
    assert_eq!(allocator.free_frames_at(8), Some(256));
}

#[test]
fn a_run_never_leaves_the_managed_range() {
    let mut allocator = released(12);
    // 12 frames cannot form an aligned run of 16, so order 4 is unavailable.
    assert_eq!(allocator.allocate(4), Err(FrameError::OutOfMemory));
    for _ in 0..3 {
        let base = allocator.allocate(2).expect("order 2 block available");
        assert!(base + 4 <= 12);
    }
}

fn heap(frames: usize) -> (SizeClassHeap, BuddyAllocator<VecStore>) {
    // Frame 0 is the null page: release the range starting at frame 1, the way
    // the kernel reserves its low memory.
    let mut frames_allocator =
        BuddyAllocator::new(VecStore::new(frames + 1), frames + 1).expect("store is large");
    frames_allocator
        .release_range(1, frames)
        .expect("release the frame range");
    (SizeClassHeap::new(), frames_allocator)
}

#[test]
fn size_classes_return_aligned_blocks() {
    let (mut heap, mut frames) = heap(32);
    let mut blocks = Vec::new();
    for size in SIZE_CLASSES {
        let ptr = heap.alloc(&mut frames, size).expect("class has a slab");
        let address = ptr.as_ptr() as usize;
        assert_eq!(address % 16, 0, "size {size} is not 16-byte aligned");
        blocks.push((address, size));
    }
    blocks.sort_unstable();
    for pair in blocks.windows(2) {
        assert!(pair[0].0 + pair[0].1 <= pair[1].0, "blocks overlap");
    }
}

#[test]
fn freeing_the_last_block_returns_its_frame() {
    let (mut heap, mut frames) = heap(8);
    let baseline = frames.free_frames();
    let ptr = heap.alloc(&mut frames, 16).expect("heap has a frame");
    assert_eq!(frames.free_frames(), baseline - 1);
    heap.dealloc(&mut frames, ptr, 16)
        .expect("block was allocated");
    assert_eq!(frames.free_frames(), baseline);
}

#[test]
fn large_requests_take_whole_frames() {
    let (mut heap, mut frames) = heap(16);
    let baseline = frames.free_frames();

    let page = heap.alloc(&mut frames, 4096).expect("frame available");
    assert_eq!((page.as_ptr() as usize) % FRAME_SIZE, 0);
    assert_eq!(frames.free_frames(), baseline - 1);

    let two_pages = heap.alloc(&mut frames, 8192).expect("two frames available");
    assert_eq!((two_pages.as_ptr() as usize) % (2 * FRAME_SIZE), 0);
    assert_eq!(frames.free_frames(), baseline - 3);

    heap.dealloc(&mut frames, page, 4096)
        .expect("block was allocated");
    heap.dealloc(&mut frames, two_pages, 8192)
        .expect("block was allocated");
    assert_eq!(frames.free_frames(), baseline);
}

#[test]
fn blocks_are_reused_after_a_free() {
    let (mut heap, mut frames) = heap(8);
    let first = heap.alloc(&mut frames, 64).expect("heap has a frame");
    heap.dealloc(&mut frames, first, 64)
        .expect("block was allocated");
    let second = heap.alloc(&mut frames, 64).expect("heap has a frame");
    assert_eq!(first, second);
}

#[test]
fn detects_a_heap_double_free() {
    let (mut heap, mut frames) = heap(8);
    let ptr = heap.alloc(&mut frames, 32).expect("heap has a frame");
    heap.dealloc(&mut frames, ptr, 32)
        .expect("block was allocated");
    assert_eq!(
        heap.dealloc(&mut frames, ptr, 32),
        Err(HeapError::DoubleFree)
    );
}

#[test]
fn reports_out_of_memory_when_frames_run_out() {
    let (mut heap, mut frames) = heap(1);
    let ptr = heap.alloc(&mut frames, 4096).expect("single frame");
    assert!(heap.alloc(&mut frames, 4096).is_none());
    heap.dealloc(&mut frames, ptr, 4096)
        .expect("block was allocated");

    // One frame gives exactly one 16-byte slab: 256 blocks, then no more.
    let mut seen = Vec::new();
    for _ in 0..(FRAME_SIZE / 16) {
        seen.push(heap.alloc(&mut frames, 16).expect("slab has a block"));
    }
    assert!(heap.alloc(&mut frames, 16).is_none());
    for ptr in seen {
        heap.dealloc(&mut frames, ptr, 16)
            .expect("block was allocated");
    }
    assert_eq!(frames.free_frames(), 1);
}

#[test]
fn rejects_zero_sized_requests() {
    let (mut heap, mut frames) = heap(4);
    assert!(heap.alloc(&mut frames, 0).is_none());
    let ptr = heap
        .alloc(&mut frames, MAX_SMALL)
        .expect("heap has a frame");
    assert_eq!(heap.dealloc(&mut frames, ptr, 0), Err(HeapError::ZeroSize));
    heap.dealloc(&mut frames, ptr, MAX_SMALL)
        .expect("block was allocated");
}
