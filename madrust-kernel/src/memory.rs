// SPDX-License-Identifier: GPL-2.0-only
//! Physical memory manager and kernel heap (milestone M2c).
//!
//! The manager is built from the PVH memory map: only RAM entries are usable.
//! The kernel image and the first MiB are taken out before the rest is released
//! to the buddy allocator.
//!
//! The buddy metadata is one byte per frame, so the store is sized from the
//! memory map at boot and carved from the top of the highest RAM region that
//! can hold it. The region is reserved before the first frame is released and
//! reached through the direct map, which covers the whole physical memory the
//! boot code mapped.
//!
//! The kernel runs from the higher half, so a physical address is no longer a
//! pointer: the frame allocator stays index based and every access to a frame
//! goes through the direct map with [`madrust_mm::phys_to_virt`].

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::fmt;
use core::ptr::NonNull;

use madrust_bootinfo::{BootInfo, HvmMemmapEntry};
use madrust_mm::buddy::BuddyAllocator;
use madrust_mm::heap::{HeapError, SizeClassHeap, MIN_ALIGN, SIZE_CLASSES};
use madrust_mm::store::SliceStore;
use madrust_mm::{phys_to_virt, FrameError, DIRECT_MAP_SPAN, FRAME_SIZE};

use crate::sync::IrqSpinLock;

/// Largest number of frames the kernel can describe.
///
/// One metadata byte per 4 KiB frame, so the direct map reservation is also the
/// metadata capacity. The boot code maps [`DIRECT_MAP_SPAN`] bytes of physical
/// memory and refuses a guest above it.
pub const MAX_FRAMES: usize = (DIRECT_MAP_SPAN / FRAME_SIZE as u64) as usize;

/// Number of blocks the boot self test allocates and frees.
pub const SELF_TEST_BLOCKS: usize = 10_000;

/// End of the reserved low memory: real-mode structures, firmware data and the
/// PVH boot structures live below 1 MiB.
const LOW_MEMORY_END: u64 = 0x10_0000;

/// Modules whose bytes are kept out of the frame allocator.
///
/// The benchmark rig passes one initramfs module. The bound keeps the reserved
/// range list a fixed size array, so the memory manager never allocates while
/// it is building itself.
const MAX_MODULES: usize = 8;

/// Reserved ranges: low memory, the kernel image, the modules and the metadata.
const MAX_RESERVED: usize = 3 + MAX_MODULES;

extern "C" {
    static __kernel_start__: u8;
    static __kernel_physical_start__: u8;
    static __kernel_physical_end__: u8;
}

/// The heap bookkeeping. It is a static so that the allocator never stores its
/// own free lists in the memory it hands out.
struct HeapCell(UnsafeCell<SizeClassHeap>);

// SAFETY: the cell is only reached through the global manager, which grants
// exclusive access for the duration of a [`with`] call.
unsafe impl Sync for HeapCell {}

static HEAP: HeapCell = HeapCell(UnsafeCell::new(SizeClassHeap::new()));

/// Physical memory manager and heap, once [`init`] has built the frames.
pub struct MemoryManager {
    frames: BuddyAllocator<SliceStore>,
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
/// The lock disables interrupts while it is held: a thread that is allocating
/// cannot be preempted by the timer handler, and the recursion guard rejects a
/// second take from the same thread (for example a nested [`with`] call).
static MANAGER_LOCK: IrqSpinLock = IrqSpinLock::new("memory manager");

/// Reason why the memory manager could not be built or the self test failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryError {
    /// The memory map does not contain a single usable RAM entry.
    NoUsableRam,
    /// The physical memory exceeds what the direct map, and therefore the
    /// metadata, can describe. `required` is the highest RAM end, `available`
    /// the span the boot code mapped.
    MemoryTooLarge { required: u64, available: u64 },
    /// No RAM region can hold the frame metadata.
    MetadataRegionUnavailable { size: usize, highest: u64 },
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
    /// The loader passed more modules than the reserved range list holds.
    TooManyModules { found: usize, capacity: usize },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoUsableRam => write!(f, "the memory map has no usable RAM"),
            Self::MemoryTooLarge {
                required,
                available,
            } => write!(
                f,
                "the guest has {} MiB of physical memory but the kernel describes at most {} MiB",
                required / (1024 * 1024),
                available / (1024 * 1024)
            ),
            Self::MetadataRegionUnavailable { size, highest } => write!(
                f,
                "no RAM region can hold the {size} byte frame metadata below 0x{highest:x}"
            ),
            _ => write!(f, "{self:?}"),
        }
    }
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

    /// Allocate one 4 KiB frame and return its physical address.
    ///
    /// Used by the user address space builder: a user page table page and a
    /// user data page are both plain frames, reached through the direct map
    /// until they are installed in the user half.
    pub fn alloc_frame(&mut self) -> Option<u64> {
        self.frames
            .allocate(0)
            .ok()
            .map(|frame| frame as u64 * FRAME_SIZE as u64)
    }

    /// Allocate one 4 KiB frame that reads back as zero.
    ///
    /// Every frame the allocator hands out holds whatever the previous owner
    /// left behind, so a mapping that requires zeroed memory uses this helper.
    pub fn alloc_zeroed_frame(&mut self) -> Option<u64> {
        let frame = self.alloc_frame()?;
        // SAFETY: the allocator just removed the frame from its free lists and
        // nothing else references it, so the zero fill is exclusive. The
        // direct map translates the physical address.
        unsafe {
            core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, FRAME_SIZE);
        }
        Some(frame)
    }

    /// Return a frame obtained from [`MemoryManager::alloc_frame`].
    pub fn free_frame(&mut self, physical: u64) -> Result<(), FrameError> {
        let frame = usize::try_from(physical / FRAME_SIZE as u64)
            .map_err(|_| FrameError::OutOfRange { frame: usize::MAX })?;
        self.frames.deallocate(frame, 0)
    }

    /// Allocate one block of `1 << order` frames and return its physical base.
    ///
    /// The buddy allocator returns a block aligned to its own size, which is
    /// what a 2 MiB page table entry needs.
    pub fn alloc_block(&mut self, order: u8) -> Option<u64> {
        self.frames
            .allocate(order)
            .ok()
            .map(|frame| frame as u64 * FRAME_SIZE as u64)
    }

    /// Allocate one block of `1 << order` frames that reads back as zero.
    pub fn alloc_zeroed_block(&mut self, order: u8) -> Option<u64> {
        let physical = self.alloc_block(order)?;
        let size = (1usize << order) * FRAME_SIZE;
        // SAFETY: the allocator just removed the whole block from its free
        // lists and nothing else references it, so the zero fill is exclusive.
        // The direct map translates the physical base.
        unsafe {
            core::ptr::write_bytes(phys_to_virt(physical) as *mut u8, 0, size);
        }
        Some(physical)
    }

    /// Return a block obtained from [`MemoryManager::alloc_block`].
    pub fn free_block(&mut self, physical: u64, order: u8) -> Result<(), FrameError> {
        let frame = usize::try_from(physical / FRAME_SIZE as u64)
            .map_err(|_| FrameError::OutOfRange { frame: usize::MAX })?;
        self.frames.deallocate(frame, order)
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
///
/// The frame metadata is carved from RAM: its size follows from the highest RAM
/// end, one byte per frame, and it is reserved before any frame is released.
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

    // The direct map covers a fixed span and the metadata describes one byte
    // per frame, so a guest above the span would stay partly unmapped. Refuse
    // it here instead of handing out frames that fault on first touch.
    if managed > MAX_FRAMES {
        return Err(MemoryError::MemoryTooLarge {
            required: highest,
            available: DIRECT_MAP_SPAN,
        });
    }

    let layout = KernelLayout::read();

    let modules = boot_info.modules();
    if modules.len() > MAX_MODULES {
        return Err(MemoryError::TooManyModules {
            found: modules.len(),
            capacity: MAX_MODULES,
        });
    }

    let mut reserved = [Range { start: 0, end: 0 }; MAX_RESERVED];
    let mut reserved_count = 0;
    reserved[reserved_count] = Range {
        start: 0,
        end: LOW_MEMORY_END,
    };
    reserved_count += 1;
    /*
     * The whole image, including `.bss`: the boot stack, the boot page tables
     * and the final page tables all live there, so one range covers every
     * kernel owned page.
     */
    reserved[reserved_count] = Range {
        start: layout.physical_start,
        end: layout.physical_end,
    };
    reserved_count += 1;
    // The loader leaves the initramfs in RAM. Its bytes are the program image,
    // so no frame the heap hands out may overlap them.
    for module in modules.iter().filter(|module| module.size != 0) {
        reserved[reserved_count] = Range {
            start: module.paddr,
            end: module.end(),
        };
        reserved_count += 1;
    }
    reserved[..reserved_count].sort_unstable_by_key(|range| range.start);

    // One metadata byte per frame, rounded up to whole pages. The region comes
    // from the top of the highest RAM region that can hold it and never
    // overlaps low memory, the image or a module.
    let metadata_size = managed;
    let metadata = place_metadata(boot_info, &reserved[..reserved_count], metadata_size).ok_or(
        MemoryError::MetadataRegionUnavailable {
            size: metadata_size,
            highest,
        },
    )?;
    reserved[reserved_count] = metadata;
    reserved_count += 1;
    reserved[..reserved_count].sort_unstable_by_key(|range| range.start);

    /*
     * SAFETY: `metadata` lies inside a RAM region that the boot information
     * reports and below `DIRECT_MAP_SPAN`, so the direct map translates every
     * page of it. The region is recorded in `reserved` and therefore never
     * released to the buddy allocator, no other code takes a reference to it,
     * and this is the only slice built over it. The allocator keeps that slice
     * exclusively for the rest of the boot, which is why `'static` is sound.
     */
    let store = unsafe {
        SliceStore::new(core::slice::from_raw_parts_mut(
            phys_to_virt(metadata.start) as *mut u8,
            metadata_size,
        ))
    };
    let mut frames = BuddyAllocator::new(store, managed).map_err(MemoryError::Frames)?;

    for region in boot_info.ram_regions() {
        release_usable(&mut frames, region, &reserved[..reserved_count])?;
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
/// again: the manager is behind an interrupt safe spin lock, and the recursion
/// guard reports the second take as a lock violation instead of deadlocking.
pub fn with<R>(f: impl FnOnce(&mut MemoryManager) -> R) -> Option<R> {
    let _guard = MANAGER_LOCK.lock();
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
/// lock itself, and the recursion guard would report a second take.
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

/// Addresses the linker script defines for the kernel image.
///
/// Both values are physical: the image is linked at a higher-half virtual
/// address and loaded at 0x100000, and the memory manager works on physical
/// frames.
struct KernelLayout {
    physical_start: u64,
    physical_end: u64,
}

impl KernelLayout {
    fn read() -> Self {
        // Linker symbols are never dereferenced: only their addresses are
        // taken, and those are defined for the whole kernel lifetime.
        Self {
            physical_start: core::ptr::addr_of!(__kernel_physical_start__) as u64,
            physical_end: core::ptr::addr_of!(__kernel_physical_end__) as u64,
        }
    }
}

/// Virtual address the kernel is linked at, as the linker script defines it.
pub fn kernel_virtual_base() -> u64 {
    core::ptr::addr_of!(__kernel_start__) as u64
}

/// Check that one physical address reads the same through the direct map as it
/// did through the identity map before the switch.
///
/// The trampoline reads `identity_value` from `physical` while the image is
/// still identity mapped; this compares it with the same word read back through
/// the direct map, so a wrong translation, a missing table entry or a stale
/// address space all show up as a mismatch or as a page fault.
pub fn direct_map_probe_matches(physical: u64, identity_value: u64) -> bool {
    // SAFETY: `physical` was read by the trampoline before the switch, so it is
    // inside RAM that the memory map reports; the direct map translates it and
    // the volatile 64-bit load matches the one the trampoline made.
    let mapped = unsafe { (phys_to_virt(physical) as *const u64).read_volatile() };
    mapped == identity_value
}

/// A half-open range of physical addresses.
#[derive(Clone, Copy)]
struct Range {
    start: u64,
    end: u64,
}

/// Carve the metadata region from the top of the highest RAM region that can
/// hold it, page aligned and clear of every reserved range.
///
/// Trying every region keeps the kernel working when the highest region is too
/// small for the metadata, for example when the loader puts a tiny RAM entry
/// above the bulk of memory.
fn place_metadata(boot_info: &BootInfo<'_>, reserved: &[Range], size: usize) -> Option<Range> {
    boot_info
        .ram_regions()
        .filter_map(|region| carve_top(region, size as u64, reserved))
        .max_by_key(|range| range.start)
}

/// Highest `size` bytes of `region` that overlap no reserved range.
///
/// Returns `None` when the region cannot hold the region at all.
fn carve_top(region: &HvmMemmapEntry, size: u64, reserved: &[Range]) -> Option<Range> {
    let page = FRAME_SIZE as u64;
    let size = size.next_multiple_of(page);
    let floor = div_ceil(region.addr, page) * page;
    let mut end = region.end() / page * page;

    loop {
        if end < floor.saturating_add(size) {
            return None;
        }
        let start = end - size;
        match reserved
            .iter()
            .find(|range| start < range.end && end > range.start)
        {
            // Slide the candidate below the range it hit. Every step lowers
            // `end`, so the loop terminates.
            Some(range) => end = range.start / page * page,
            None => return Some(Range { start, end }),
        }
    }
}

/// Release the part of one RAM region that no reserved range covers.
fn release_usable(
    frames: &mut BuddyAllocator<SliceStore>,
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
