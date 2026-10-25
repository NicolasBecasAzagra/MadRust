// SPDX-License-Identifier: GPL-2.0-only
//! Per processor state (milestone E3b).
//!
//! Every processor owns one [`PerCpu`] block. The block is the `GS` base of
//! that processor, allocated from the kernel heap and page aligned, so the
//! system call stub can reach it through `swapgs` and the scheduler can reach
//! its own run queue without a global lock.
//!
//! ## Layout
//!
//! | Offset | Field | Reader |
//! |--------|-------|--------|
//! | `0x00` | `user_stack` | `syscall_entry.s` at `GS:0` |
//! | `0x08` | `user_frame` | `syscall_entry.s` at `GS:8` |
//! | `0x10` | `tss_base` | `syscall_entry.s` at `GS:16` |
//! | `0x18` | `self_base` | [`this`] |
//! | `0x20` | `index` | the kernel |
//! | `0x28` | `apic_id` | the kernel |
//! | `0x30` | `online` | the kernel |
//! | `0x38` | `heartbeat` | the timer handler |
//! | `0x40` | `current` | the scheduler |
//! | `0x48` | `finished` | the scheduler |
//! | `0x50` | `idle_thread` | the scheduler |
//! | `0x58` | `last_out` | the context switch |
//!
//! The three fields at `GS:0`, `GS:8` and `GS:16` are fixed by the assembly and
//! checked at compile time. Everything after them is Rust private state.
//!
//! ## Why no global lock
//!
//! A processor only reads and writes its own block. The run queue and the slice
//! counter are reached with interrupts disabled on that processor, so the timer
//! handler cannot re-enter them. The only cross processor access is a wakeup
//! that pushes a thread into another processor's queue; that path takes the
//! target block's own queue lock, never a lock shared by every processor.

use core::cell::UnsafeCell;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use madrust_arch::cpu;
use madrust_sched::{RunQueue, Slice};

use crate::memory;
use crate::sync::{IrqLockGuard, IrqSpinLock};

/// Largest number of processors the kernel describes.
///
/// The thread table reserves one idle slot per processor and the GDT one TSS
/// descriptor per processor, so the bound is part of both layouts.
pub const MAX_CPUS: usize = 8;

/// Sentinel stored in [`PerCpu::current`] when no thread is installed.
pub const NO_THREAD: usize = usize::MAX;

/// Size of one per processor block.
///
/// The block is far smaller than a page, but a page aligned allocation keeps
/// the `GS` base aligned and makes the layout easy to read in a debugger.
pub const PER_CPU_SIZE: usize = 4096;

/// `IA32_GS_BASE`: the active `GS` base.
const MSR_GS_BASE: u32 = 0xc000_0101;
/// `IA32_KERNEL_GS_BASE`: the `GS` base `swapgs` installs.
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

/// State that belongs to exactly one processor.
/// Number of departed threads a processor can remember.
///
/// A processor has one runnable thread at a time, so the number of threads
/// that can be waiting to come back is the number of runnable threads, which
/// the queue bounds.
pub const DEPARTED_SLOTS: usize = 8;

#[repr(C, align(64))]
pub struct PerCpu {
    /// User stack pointer saved by the system call stub at `GS:0`.
    pub user_stack: UnsafeCell<u64>,
    /// Register frame saved by the system call stub at `GS:8`.
    pub user_frame: UnsafeCell<u64>,
    /// Address of the TSS of this processor, read by the system call stub at
    /// `GS:16` to find `rsp0`.
    pub tss_base: AtomicU64,
    /// Address of this block, so [`this`] needs one `GS` load, not an MSR read.
    pub self_base: AtomicU64,
    /// Index of this processor in [`BASES`].
    pub index: usize,
    /// Local APIC identifier, the destination of an inter processor interrupt.
    pub apic_id: AtomicU32,
    /// Set when the processor finished its initialisation and runs its idle
    /// loop. A processor that failed to start stays false.
    pub online: AtomicBool,
    /// Timer interrupts observed since the processor came online.
    pub heartbeat: AtomicU64,
    /// Thread that owns this processor, or [`NO_THREAD`] before init.
    pub current: AtomicUsize,
    /// Threads that retired on this processor.
    pub finished: AtomicUsize,
    /// Thread table slot of the idle thread of this processor.
    pub idle_thread: AtomicUsize,
    /// Threads the switch left behind, cleared when they resume.
    ///
    /// A ring, not a single slot: two threads can leave this processor
    /// before either comes back, and with a single slot the first one would
    /// keep its `on_cpu` for ever, which stops the thread table from being
    /// reclaimed.
    pub departed: [AtomicUsize; DEPARTED_SLOTS],
    /// Index of the next slot to write.
    pub depart_head: AtomicUsize,
    /// Index of the next slot to read.
    pub depart_tail: AtomicUsize,
    /// Serialises the run queue of this processor only.
    queue_lock: IrqSpinLock,
    /// Threads that may run on this processor.
    queue: UnsafeCell<RunQueue>,
    /// Slice counter of the thread that is running here.
    slice: UnsafeCell<Slice>,
}

// SAFETY: the immutable fields are written once before the processor starts
// and the mutable fields are atomic or reached only by the owner of this
// processor with interrupts disabled. The run queue has its own lock for the
// one cross processor push.
unsafe impl Sync for PerCpu {}

impl PerCpu {
    /// Record that a thread left this processor.
    ///
    /// The slot is written before the index is published, so a reader that
    /// sees the index also sees the thread.
    pub fn record_departure(&self, thread: usize) {
        let head = self.depart_head.load(Ordering::Relaxed);
        self.departed[head % DEPARTED_SLOTS].store(thread, Ordering::Release);
        self.depart_head.store(head + 1, Ordering::Release);
    }

    /// The threads that left this processor and have not come back yet.
    ///
    /// They are handed to a callback instead of a vector: the kernel does not
    /// allocate to report a list.
    pub fn take_departures(&self, mut visit: impl FnMut(usize)) {
        loop {
            let tail = self.depart_tail.load(Ordering::Acquire);
            let thread = self.departed[tail % DEPARTED_SLOTS].load(Ordering::Acquire);
            if thread == NO_THREAD {
                break;
            }
            self.departed[tail % DEPARTED_SLOTS].store(NO_THREAD, Ordering::Release);
            self.depart_tail.store(tail + 1, Ordering::Release);
            visit(thread);
        }
    }

    /// Empty block for one processor.
    pub const fn new(index: usize) -> Self {
        Self {
            user_stack: UnsafeCell::new(0),
            user_frame: UnsafeCell::new(0),
            tss_base: AtomicU64::new(0),
            self_base: AtomicU64::new(0),
            index,
            apic_id: AtomicU32::new(0),
            online: AtomicBool::new(false),
            heartbeat: AtomicU64::new(0),
            current: AtomicUsize::new(NO_THREAD),
            finished: AtomicUsize::new(0),
            idle_thread: AtomicUsize::new(NO_THREAD),
            departed: [const { AtomicUsize::new(NO_THREAD) }; DEPARTED_SLOTS],
            depart_head: AtomicUsize::new(0),
            depart_tail: AtomicUsize::new(0),
            queue_lock: IrqSpinLock::new("run queue"),
            queue: UnsafeCell::new(RunQueue::new()),
            slice: UnsafeCell::new(Slice::new(crate::sched::SLICE_TICKS)),
        }
    }

    /// Address of this block, the value the CPU keeps in `IA32_GS_BASE`.
    pub fn base(&self) -> u64 {
        self as *const Self as u64
    }

    /// Take the run queue lock of this processor.
    ///
    /// Interrupts are disabled for as long as the guard lives. The caller must
    /// drop the guard before it switches threads.
    pub fn lock_queue(&self) -> IrqLockGuard<'_> {
        self.queue_lock.lock()
    }

    /// Exclusive view of the run queue.
    ///
    /// # Safety
    ///
    /// The caller must hold [`PerCpu::lock_queue`] and must not switch threads
    /// while the borrow lives.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn queue_mut(&self) -> &mut RunQueue {
        // SAFETY: the caller holds the queue lock, which is the only guard on
        // this field, and no borrow survives a context switch.
        unsafe { &mut *self.queue.get() }
    }

    /// Exclusive view of the slice counter.
    ///
    /// # Safety
    ///
    /// The caller runs on this processor with interrupts disabled and does not
    /// switch threads while the borrow lives.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn slice_mut(&self) -> &mut Slice {
        // SAFETY: interrupts are disabled, so the timer handler and the
        // scheduler cannot both hold this borrow.
        unsafe { &mut *self.slice.get() }
    }
}

const _: () = assert!(offset_of!(PerCpu, user_stack) == 0);
const _: () = assert!(offset_of!(PerCpu, user_frame) == 8);
const _: () = assert!(offset_of!(PerCpu, tss_base) == 16);
const _: () = assert!(offset_of!(PerCpu, self_base) == 24);
const _: () = assert!(size_of::<PerCpu>() <= PER_CPU_SIZE);

/// Static block of the boot processor, available before the heap exists.
struct BspCell(UnsafeCell<PerCpu>);

// SAFETY: only [`this`] reaches the block, and it only hands out `&PerCpu`
// while the owner keeps the mutation rules of the type.
unsafe impl Sync for BspCell {}

static BSP: BspCell = BspCell(UnsafeCell::new(PerCpu::new(0)));

/// Address of every block, indexed by processor index.
static BASES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Point `GS` at the block of the boot processor.
///
/// Called before the first lock is taken. Loading a data segment later resets
/// the `GS` base, so [`crate::gdt`] calls [`load_base`] again after it loads
/// the descriptor table.
pub fn init_bsp() {
    let base = unsafe { &*BSP.0.get() }.base();
    unsafe { &*BSP.0.get() }
        .self_base
        .store(base, Ordering::Release);
    BASES[0].store(base, Ordering::Release);
    load_base(0);
}

/// Install the `GS` bases of processor `index`.
///
/// Both MSRs get the same value. In this milestone the kernel never uses a
/// separate user `GS` base: the user runtime uses `FS` for thread local
/// storage, so `swapgs` in the system call stub is a no-op and `GS` always
/// names the local block.
pub fn load_base(index: usize) {
    let Some(base) = BASES.get(index).map(|slot| slot.load(Ordering::Acquire)) else {
        return;
    };
    if base == 0 {
        return;
    }
    // SAFETY: the address is one of the blocks this module owns, and both MSRs
    // accept the canonical kernel address.
    unsafe {
        cpu::write_msr(MSR_GS_BASE, base);
        cpu::write_msr(MSR_KERNEL_GS_BASE, base);
    }
}

/// Block of the processor that is running this code.
pub fn this() -> &'static PerCpu {
    let base: u64;
    // SAFETY: `GS` names a block of this module in kernel mode, and the word at
    // offset 24 is the address of that same block. The load has no side effect
    // and the block lives for the rest of the boot.
    unsafe {
        core::arch::asm!(
            "mov {base}, gs:[24]",
            base = out(reg) base,
            options(nostack, readonly, preserves_flags),
        );
        &*(base as *const PerCpu)
    }
}

/// Block of processor `index`, when it has been created.
pub fn get(index: usize) -> Option<&'static PerCpu> {
    let base = BASES.get(index)?.load(Ordering::Acquire);
    if base == 0 {
        None
    } else {
        // SAFETY: same invariant as [`this`].
        Some(unsafe { &*(base as *const PerCpu) })
    }
}

/// Allocate the block of processor `index` and return it.
///
/// The allocation is page aligned, so the returned reference is aligned to the
/// declared alignment of [`PerCpu`].
pub fn allocate(index: usize) -> Option<&'static PerCpu> {
    let block = memory::with(|manager| manager.alloc(PER_CPU_SIZE))??;
    let pointer = block.as_ptr() as *mut PerCpu;
    if !(pointer as usize).is_multiple_of(core::mem::align_of::<PerCpu>()) {
        return None;
    }
    // SAFETY: the heap returned a block of `PER_CPU_SIZE` bytes that no other
    // code references. Writing the initialised value over the uninitialised
    // block is the only construction of the type.
    unsafe {
        pointer.write(PerCpu::new(index));
        (*pointer)
            .self_base
            .store(pointer as u64, Ordering::Release);
    }
    BASES[index].store(pointer as u64, Ordering::Release);
    // SAFETY: the block is initialised and lives for the rest of the boot.
    Some(unsafe { &*pointer })
}

/// Record one timer interrupt on the calling processor.
pub fn record_heartbeat() {
    this().heartbeat.fetch_add(1, Ordering::Relaxed);
}
