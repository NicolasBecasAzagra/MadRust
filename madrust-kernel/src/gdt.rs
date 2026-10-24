// SPDX-License-Identifier: GPL-2.0-only
//! Global descriptor table, task state segment and interrupt stacks.
//!
//! The table keeps the flat kernel code and data descriptors loaded by the
//! 32-bit trampoline and adds a 64-bit TSS descriptor. The TSS holds the ring-0
//! stack pointer and IST1, which the double fault gate uses so that a stack
//! fault remains diagnosable.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut};

/// Selector of the 64-bit code descriptor.
pub const CODE_SELECTOR: u16 = 0x08;
/// Selector of the 64-bit data descriptor.
pub const DATA_SELECTOR: u16 = 0x10;
/// Selector of the 64-bit TSS descriptor.
const TSS_SELECTOR: u16 = 0x18;

/// IST index used by the double fault gate.
pub const DOUBLE_FAULT_IST: u8 = 1;

const GDT_ENTRIES: usize = 5;

#[repr(C, align(8))]
struct Gdt {
    entries: [u64; GDT_ENTRIES],
}

struct GdtCell(UnsafeCell<Gdt>);

// SAFETY: the GDT is written once during single-threaded early boot, before
// interrupts are enabled, and is only read by the CPU afterwards.
unsafe impl Sync for GdtCell {}

static GDT: GdtCell = GdtCell(UnsafeCell::new(Gdt {
    entries: [
        0x0000_0000_0000_0000, // null
        0x00AF_9A00_0000_FFFF, // 64-bit code, ring 0, base 0, limit 4 GiB
        0x00AF_9200_0000_FFFF, // 64-bit data, ring 0, base 0, limit 4 GiB
        0,                     // TSS low half, filled in by `init`
        0,                     // TSS high half, filled in by `init`
    ],
}));

/// 64-bit task state segment.
///
/// The layout is fixed by the architecture, including the unaligned offsets,
/// so the structure is packed and every field is written through a raw
/// pointer.
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

const TSS_ZERO: Tss = Tss {
    reserved0: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    reserved1: 0,
    ist: [0; 7],
    reserved2: 0,
    reserved3: 0,
    iomap_base: 0,
};

struct TssCell(UnsafeCell<Tss>);

// SAFETY: the TSS is written once during single-threaded early boot, before
// interrupts are enabled, and is only read by the CPU afterwards.
unsafe impl Sync for TssCell {}

static TSS: TssCell = TssCell(UnsafeCell::new(TSS_ZERO));

const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct DoubleFaultStack([u8; DOUBLE_FAULT_STACK_SIZE]);

struct DoubleFaultStackCell(UnsafeCell<DoubleFaultStack>);

// SAFETY: the double fault stack is only used by the CPU when vector 8 is
// delivered on IST1. Nothing else aliases it.
unsafe impl Sync for DoubleFaultStackCell {}

static DOUBLE_FAULT_STACK: DoubleFaultStackCell = DoubleFaultStackCell(UnsafeCell::new(
    DoubleFaultStack([0; DOUBLE_FAULT_STACK_SIZE]),
));

extern "C" {
    /// Top of the kernel stack built by the 32-bit trampoline.
    static __stack_top: u8;
}

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// Build a 64-bit TSS descriptor split into its two GDT slots.
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (size_of::<Tss>() - 1) as u64;
    // Type 0x9 (64-bit TSS available), S = 0, DPL = 0, present.
    const TYPE_PRESENT_RING0: u64 = 0x89;

    let low = (limit & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (TYPE_PRESENT_RING0 << 40)
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let high = base >> 32;

    (low, high)
}

/// Install the GDT, initialise the TSS and load the task register.
pub fn init() {
    let tss = TSS.0.get();
    let stack = DOUBLE_FAULT_STACK.0.get();

    // SAFETY: `tss` and `stack` point at static storage owned by this module
    // and nothing else accesses it during boot. The TSS starts zeroed, so only
    // the non-zero fields need an unaligned store.
    unsafe {
        let ist_top = addr_of!(*stack) as u64 + size_of::<DoubleFaultStack>() as u64;
        let kernel_top = addr_of!(__stack_top) as u64;
        core::ptr::write_unaligned(addr_of_mut!((*tss).rsp0), kernel_top);
        core::ptr::write_unaligned(addr_of_mut!((*tss).ist[0]), ist_top);
        core::ptr::write_unaligned(addr_of_mut!((*tss).iomap_base), size_of::<Tss>() as u16);
    }

    let tss_base = tss as u64;
    let (tss_low, tss_high) = tss_descriptor(tss_base);

    let gdt = GDT.0.get();
    // SAFETY: `gdt` points at the static GDT above; entries 3 and 4 are the two
    // halves of the TSS descriptor and are not read concurrently.
    unsafe {
        (*gdt).entries[3] = tss_low;
        (*gdt).entries[4] = tss_high;
    }

    let pointer = GdtPointer {
        limit: (size_of::<Gdt>() - 1) as u16,
        base: gdt as u64,
    };

    // SAFETY: `pointer` describes the fully populated static GDT above and
    // `lgdt` reads exactly the ten bytes of the descriptor.
    unsafe {
        asm!("lgdt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }

    // SAFETY: selector 0x10 is the ring-0 data descriptor in the trampoline GDT
    // and in this one, so loading it cannot fault.
    unsafe {
        asm!(
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "mov fs, ax",
            "mov gs, ax",
            in("ax") DATA_SELECTOR,
            options(nomem, nostack, preserves_flags),
        );
    }

    // SAFETY: selector 0x18 was just installed as a present, ring-0, available
    // 64-bit TSS descriptor whose base points at the initialised TSS.
    unsafe {
        asm!("ltr ax", in("ax") TSS_SELECTOR, options(nostack));
    }
}
