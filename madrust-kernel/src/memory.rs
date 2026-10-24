// SPDX-License-Identifier: GPL-2.0-only
//! Physical memory manager and kernel heap (milestone M2a).
//!
//! The manager is built from the PVH memory map: only RAM entries are usable.
//! The kernel image, the boot page tables, the boot stack and the first MiB are
//! reserved before the rest is released to the buddy allocator.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use madrust_bootinfo::{BootInfo, HvmMemmapEntry};
use madrust_mm::buddy::BuddyAllocator;
use madrust_mm::heap::{HeapError, SizeClassHeap, MIN_ALIGN, SIZE_CLASSES};
use madrust_mm::store::StaticStore;
use madrust_mm::{FrameError, FRAME_SIZE};

/// Frames the static metadata can describe: 1 GiB of physical memory.
pub const MAX_FRAMES: usize = 262_144;

/// Number of blocks the boot self test allocates and frees.
pub const SELF_TEST_BLOCKS: usize = 10_000;

/// End of the reserved low memory: real-mode structures, firmware data and the
/// null page live below 1 MiB.
const LOW_MEMORY_END: u64 = 0x10_0000;

extern "C" {
    static __kernel_physical_start__: u8;
    static __kernel_physical_end__: u8;
    static __stack_bottom: u8;
    static __stack_top: u8;
    static boot_pml4: u8;
    static boot_pd: u8;
}

/// One byte of buddy metadata per physical frame.
struct MetadataCell(UnsafeCell<[u8; MAX_FRAMES]>);

// SAFETY: the buffer is handed out exactly once, as an exclusive slice, by
// [`init`] before any other code can reach it; nothing else aliases it.
unsafe impl Sync for MetadataCell {}

static METADATA: MetadataCell = MetadataCell(UnsafeCell::new([0; MAX_FRAMES]));

/// The heap bookkeeping. It is a static so that the allocator never stores its
/// own free lists in the memory it hands out.
struct HeapCell(UnsafeCell<SizeClassHeap>);

// SAFETY: the cell is only reached through the global manager, which grants
// exclusive access for the duration of a [`with`] call.
unsafe impl Sync for HeapCell {}

static HEAP: HeapCell = HeapCell(UnsafeCell::new(SizeClassHeap::new()));

/// Physical memory manager and heap, once [`init`] has built the frames.
pub struct MemoryManager {
    frames: BuddyAllocator<StaticStore>,
    heap: &'static mut SizeClassHeap,
}

struct ManagerCell(UnsafeCell<Option<MemoryManager>>);

// SAFETY: the manager is written once during single-threaded boot and is only
// reached through [`with`], which holds [`MANAGER_LOCK`] while a caller has the
// exclusive borrow.
unsafe impl Sync for ManagerCell {}

static MANAGER: ManagerCell = ManagerCell(UnsafeCell::new(None));

/// Guards the manager so that the global allocator can be called from anywhere.
///
/// Interrupt handlers never allocate, so holding the lock with interrupts
/// enabled cannot deadlock.
struct ManagerLock(AtomicBool);

impl ManagerLock {
    const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Take the lock, spinning until it is free.
    fn acquire(&self) -> ManagerGuard<'_> {
        while self
            .0
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        ManagerGuard(&self.0)
    }
}

static MANAGER_LOCK: ManagerLock = ManagerLock::new();

/// Releases [`MANAGER_LOCK`] when it goes out of scope.
struct ManagerGuard<'a>(&'a AtomicBool);

impl Drop for ManagerGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Reason why the memory manager could not be built or the self test failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryError {
    /// The memory map does not contain a single usable RAM entry.
    NoUsableRam,
    /// The physical memory exceeds what the static metadata describes.
    MetadataTooSmall { required: usize, capacity: usize },
    /// The frame allocator rejected an operation.
    Frames(FrameError),
    /// The heap rejected an operation.
    Heap(HeapError),
    /// The manager was used before [`init`] succeeded.
    NotInitialised,
    /// The self test could not allocate a block.
    AllocationFailed { size: usize, iteration: usize },
    /// The self test received a pointer below the documented alignment.
    MisalignedBlock { iteration: usize },
    /// The self test read back a different pattern than it wrote.
    BlockCorrupted { iteration: usize, offset: usize },
    /// The self test did not return the frame count to its baseline.
    LeakedFrames { expected: usize, found: usize },
    /// A `Vec` or `String` allocated through the global allocator is wrong.
    GlobalAllocatorFailed,
}

impl MemoryManager {
    /// Allocate `size` bytes from the kernel heap.
    pub fn alloc(&mut self, size: usize) -> Option<NonNull<u8>> {
        self.heap.alloc(&mut self.frames, size)
    }

    /// Release a block obtained from [`MemoryManager::alloc`].
    pub fn dealloc(&mut self, block: NonNull<u8>, size: usize) -> Result<(), HeapError> {
        self.heap.dealloc(&mut self.frames, block, size)
    }

    /// Number of free physical frames.
    pub fn free_frames(&self) -> usize {
        self.frames.free_frames()
    }

    /// Number of frames the buddy allocator manages.
    pub fn managed_frames(&self) -> usize {
        self.frames.managed_frames()
    }
}

/// Build the physical manager from the boot memory map.
pub fn init(boot_info: &BootInfo<'_>) -> Result<(), MemoryError> {
    let highest = boot_info
        .ram_regions()
        .map(HvmMemmapEntry::end)
        .max()
        .unwrap_or(0);
    if highest == 0 {
        return Err(MemoryError::NoUsableRam);
    }

    let managed = div_ceil(highest, FRAME_SIZE as u64) as usize;
    if managed > MAX_FRAMES {
        return Err(MemoryError::MetadataTooSmall {
            required: managed,
            capacity: MAX_FRAMES,
        });
    }

    let layout = KernelLayout::read();

    // SAFETY: the metadata buffer is handed out here, once, as an exclusive
    // slice, and no other code keeps a reference to it.
    let metadata: &'static mut [u8] = unsafe { &mut *METADATA.0.get() };
    let mut frames =
        BuddyAllocator::new(StaticStore::new(metadata), managed).map_err(MemoryError::Frames)?;

    let mut reserved = [
        Range {
            start: 0,
            end: LOW_MEMORY_END,
        },
        Range {
            start: layout.physical_start,
            end: layout.physical_end,
        },
        Range {
            start: layout.page_tables_start,
            end: layout.page_tables_end,
        },
        Range {
            start: layout.stack_start,
            end: layout.stack_end,
        },
    ];
    reserved.sort_unstable_by_key(|range| range.start);

    for region in boot_info.ram_regions() {
        release_usable(&mut frames, region, &reserved)?;
    }

    // SAFETY: the heap cell is aliased here for the first and only time, and
    // the manager keeps that exclusive reference for the rest of the boot.
    let heap = unsafe { &mut *HEAP.0.get() };
    let manager = MemoryManager { frames, heap };

    // SAFETY: the manager slot is empty during boot and is written once, after
    // the single-threaded init path. No other reference to the slot exists.
    unsafe {
        *MANAGER.0.get() = Some(manager);
    }

    Ok(())
}

/// Run `f` with the global memory manager.
///
/// Returns `None` before [`init`] succeeded. The closure must not call [`with`]
/// again: the manager is behind a spin lock, so a nested call would deadlock.
pub fn with<R>(f: impl FnOnce(&mut MemoryManager) -> R) -> Option<R> {
    let _guard = MANAGER_LOCK.acquire();
    // SAFETY: the manager is created once during single-threaded boot and this
    // function is the only way to reach it. The lock excludes every other
    // caller, so the exclusive borrow cannot overlap.
    let slot = unsafe { &mut *MANAGER.0.get() };
    slot.as_mut().map(f)
}

/// Boot self test for the frame allocator and the heap.
///
/// Allocates and frees [`SELF_TEST_BLOCKS`] blocks in a round robin over the
/// size classes, with one 4096-byte and one 8192-byte request in the middle,
/// checks that the frame count returns to its baseline, and finally exercises
/// the global allocator with a `Vec` and a `String`.
pub fn self_test() -> Result<(), MemoryError> {
    let baseline = with(|manager| manager.free_frames()).ok_or(MemoryError::NotInitialised)?;

    with(|manager| {
        for iteration in 0..SELF_TEST_BLOCKS {
            let size = match iteration {
                4_000 => FRAME_SIZE,
                8_000 => 2 * FRAME_SIZE,
                _ => SIZE_CLASSES[iteration % SIZE_CLASSES.len()],
            };

            let block = manager
                .alloc(size)
                .ok_or(MemoryError::AllocationFailed { size, iteration })?;
            if !(block.as_ptr() as usize).is_multiple_of(MIN_ALIGN) {
                return Err(MemoryError::MisalignedBlock { iteration });
            }

            // Touch every byte: a frame that is not mapped and writable would
            // fault here instead of corrupting a later allocation.
            let pattern = iteration as u8;
            for offset in 0..size {
                // SAFETY: `alloc` returned a block of at least `size` bytes,
                // identity mapped and not aliased by any other allocation.
                unsafe { block.as_ptr().add(offset).write_volatile(pattern) };
            }
            for offset in 0..size {
                // SAFETY: same block and range, just written above.
                if unsafe { block.as_ptr().add(offset).read_volatile() } != pattern {
                    return Err(MemoryError::BlockCorrupted { iteration, offset });
                }
            }

            manager.dealloc(block, size).map_err(MemoryError::Heap)?;
        }

        let after = manager.free_frames();
        if after != baseline {
            return Err(MemoryError::LeakedFrames {
                expected: baseline,
                found: after,
            });
        }
        Ok(())
    })
    .ok_or(MemoryError::NotInitialised)??;

    global_allocator_test()
}

/// Exercise the global allocator that the `alloc` crate uses.
///
/// Runs outside [`with`] on purpose: the global allocator takes the manager
/// lock itself, so calling it while the lock is held would deadlock.
fn global_allocator_test() -> Result<(), MemoryError> {
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::fmt::Write as _;

    let mut numbers: Vec<u32> = Vec::new();
    for value in 0..64u32 {
        numbers.push(value);
    }
    if numbers.len() != 64 || numbers[63] != 63 {
        return Err(MemoryError::GlobalAllocatorFailed);
    }

    let mut text = String::new();
    let _ = write!(text, "MadRust heap {}", numbers.len());
    if text.as_str() != "MadRust heap 64" {
        return Err(MemoryError::GlobalAllocatorFailed);
    }

    // Dropping both releases their blocks back to the heap.
    Ok(())
}

/// Addresses the linker script defines for the kernel image and its boot data.
///
/// Only the load addresses are used: M2a runs identity mapped, so a physical
/// address is a valid virtual address. M2b replaces the direct use of these
/// values with a translation through the direct map.
struct KernelLayout {
    physical_start: u64,
    physical_end: u64,
    page_tables_start: u64,
    page_tables_end: u64,
    stack_start: u64,
    stack_end: u64,
}

impl KernelLayout {
    fn read() -> Self {
        // Linker symbols are never dereferenced: only their addresses are
        // taken, and those are defined for the whole kernel lifetime.
        Self {
            physical_start: core::ptr::addr_of!(__kernel_physical_start__) as u64,
            physical_end: core::ptr::addr_of!(__kernel_physical_end__) as u64,
            page_tables_start: core::ptr::addr_of!(boot_pml4) as u64,
            page_tables_end: core::ptr::addr_of!(boot_pd) as u64 + FRAME_SIZE as u64,
            stack_start: core::ptr::addr_of!(__stack_bottom) as u64,
            stack_end: core::ptr::addr_of!(__stack_top) as u64,
        }
    }
}

/// A half-open range of physical addresses.
#[derive(Clone, Copy)]
struct Range {
    start: u64,
    end: u64,
}

/// Release the part of one RAM region that no reserved range covers.
fn release_usable(
    frames: &mut BuddyAllocator<StaticStore>,
    region: &HvmMemmapEntry,
    reserved: &[Range],
) -> Result<(), MemoryError> {
    let page = FRAME_SIZE as u64;
    let first = div_ceil(region.addr, page);
    let end = region.end() / page;
    if end <= first {
        return Ok(());
    }
    let end = end as usize;
    let mut cursor = first as usize;

    for range in reserved {
        // A frame that is only partially reserved stays reserved, hence the
        // floor at the start and the ceiling at the end.
        let start = (range.start / page) as usize;
        let stop = div_ceil(range.end, page) as usize;
        if stop <= cursor || start >= end {
            continue;
        }
        if start > cursor {
            let stop = start.min(end);
            frames
                .release_range(cursor, stop - cursor)
                .map_err(MemoryError::Frames)?;
        }
        cursor = cursor.max(stop);
        if cursor >= end {
            break;
        }
    }

    if cursor < end {
        frames
            .release_range(cursor, end - cursor)
            .map_err(MemoryError::Frames)?;
    }
    Ok(())
}

/// `value / divisor` rounded up, without overflowing on the addition.
fn div_ceil(value: u64, divisor: u64) -> u64 {
    value / divisor + u64::from(!value.is_multiple_of(divisor))
}

/// Global allocator that forwards the `alloc` crate to the kernel heap.
struct GlobalHeap;

// SAFETY: every method goes through `with`, which holds the manager lock, so
// concurrent calls are serialised. A request whose alignment exceeds the heap
// guarantee reports failure instead of returning a misaligned block.
unsafe impl GlobalAlloc for GlobalHeap {
    // SAFETY: the caller guarantees a valid `layout`. The heap returns a block
    // of at least `layout.size()` bytes or null, as the trait requires.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > MIN_ALIGN {
            return core::ptr::null_mut();
        }
        match with(|manager| manager.alloc(layout.size())) {
            Some(Some(block)) => block.as_ptr(),
            _ => core::ptr::null_mut(),
        }
    }

    // SAFETY: `ptr` comes from `alloc` with the same `layout`, as the
    // `GlobalAlloc` contract requires, so the size matches the block.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(block) = NonNull::new(ptr) else {
            return;
        };
        let _ = with(|manager| manager.dealloc(block, layout.size()));
    }
}

#[global_allocator]
static GLOBAL_HEAP: GlobalHeap = GlobalHeap;

/// Called by the `alloc` crate when the heap cannot satisfy a request.
#[alloc_error_handler]
fn allocation_error(layout: Layout) -> ! {
    crate::kprintln!(
        "MadRust: allocation FAILED: {} bytes, align {}",
        layout.size(),
        layout.align()
    );
    let uart = crate::console::console();
    uart.flush();
    crate::debug_exit::exit(crate::debug_exit::EXIT_MEMORY_FAILED)
}
