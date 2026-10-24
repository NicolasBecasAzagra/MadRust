// SPDX-License-Identifier: GPL-2.0-only
//! Global descriptor table, task state segment and kernel stacks.
//!
//! The table keeps the flat kernel code and data descriptors loaded by the
//! 32-bit trampoline, adds the two ring 3 descriptors, and ends with a 64-bit
//! TSS descriptor. The TSS holds the ring-0 stack pointer (`rsp0`) that the CPU
//! installs when an interrupt or a `syscall` arrives from ring 3, plus IST1,
//! which the double fault gate uses so that a stack fault remains diagnosable.
//!
//! `rsp0` points at a dedicated static stack: the boot stack belongs to the
//! kernel thread that starts the user program, and a user program must not be
//! able to grow its own frames over that thread's state.
//!
//! ## Selector layout
//!
//! | Selector | Descriptor |
//! |----------|------------|
//! | `0x08`   | ring 0 code |
//! | `0x10`   | ring 0 data |
//! | `0x1B`   | ring 3 code, used by the `iretq` that enters user mode |
//! | `0x23`   | ring 3 data |
//! | `0x2B`   | ring 3 code, the selector `sysretq` returns to |
//! | `0x30`   | TSS |
//!
//! `SYSRET` derives its selectors from `IA32_STAR[63:48]`: code is that field
//! plus 16 and stack is that field plus 8, both with RPL 3. With the data
//! selector at `0x23` the field is `0x1B`, so the returned stack selector is
//! `0x23` and the returned code selector is `0x2B`. The alias at `0x28` is why
//! the table has one more entry than the descriptors alone need.

use core::arch::asm;
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

/// Selector of the 64-bit ring 0 code descriptor.
pub const CODE_SELECTOR: u16 = 0x08;
/// Selector of the 64-bit ring 0 data descriptor.
pub const DATA_SELECTOR: u16 = 0x10;
/// Selector of the 64-bit ring 3 code descriptor.
pub const USER_CODE_SELECTOR: u16 = 0x1b;
/// Selector of the 64-bit ring 3 data descriptor.
pub const USER_DATA_SELECTOR: u16 = 0x23;
/// Code selector `sysretq` returns to, as derived from `IA32_STAR`.
pub const SYSRET_CODE_SELECTOR: u16 = 0x2b;
/// Selector of the 64-bit TSS descriptor.
const TSS_SELECTOR: u16 = 0x30;

/// IST index used by the double fault gate.
pub const DOUBLE_FAULT_IST: u8 = 1;

// The selectors `SYSRET` derives from `IA32_STAR` must match the table above:
// the instruction returns to code `STAR[63:48] + 16` and stack
// `STAR[63:48] + 8`, both with RPL 3, and `init` sets `STAR[63:48]` to
// `USER_CODE_SELECTOR`. A layout that breaks this would fault on the first
// system call return, so the relation is checked at compile time.
const _: () = assert!(SYSRET_CODE_SELECTOR == USER_CODE_SELECTOR + 16);
const _: () = assert!(USER_DATA_SELECTOR == USER_CODE_SELECTOR + 8);

const GDT_ENTRIES: usize = 8;

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
        0x00AF_FA00_0000_FFFF, // 64-bit code, ring 3
        0x00AF_F200_0000_FFFF, // 64-bit data, ring 3
        0x00AF_FA00_0000_FFFF, // 64-bit code, ring 3, SYSRET target
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
// interrupts are enabled, and is only read by the CPU afterwards. The syscall
// stub reads `rsp0`, which never changes again.
unsafe impl Sync for TssCell {}

static TSS: TssCell = TssCell(UnsafeCell::new(TSS_ZERO));

/// Address of the TSS, published for the assembly system call entry.
///
/// The stub reads this word to find the TSS and then loads `rsp0` from it, so
/// the kernel stack it switches to is the one the CPU installs for a ring 3
/// interrupt. The value is written once by [`init`], before the first system
/// call can happen.
#[no_mangle]
pub static MADRUST_TSS_ADDRESS: AtomicU64 = AtomicU64::new(0);

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

const INTERRUPT_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct InterruptStack([u8; INTERRUPT_STACK_SIZE]);

struct InterruptStackCell(UnsafeCell<InterruptStack>);

// SAFETY: the stack is the `rsp0` target, so only the CPU touches it, and only
// after a transition from ring 3. Nothing else aliases it.
unsafe impl Sync for InterruptStackCell {}

static INTERRUPT_STACK: InterruptStackCell =
    InterruptStackCell(UnsafeCell::new(InterruptStack([0; INTERRUPT_STACK_SIZE])));

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

/// Address one past the last byte of the static ring 0 stack.
pub fn interrupt_stack_top() -> u64 {
    addr_of!(*INTERRUPT_STACK.0.get()) as u64 + size_of::<InterruptStack>() as u64
}

/// Point `TSS.rsp0` at the kernel stack of the thread that is about to run.
///
/// The CPU reads this field on every transition from ring 3, so an interrupt or
/// a system call of a user thread lands on that thread's own stack instead of a
/// stack shared by every thread.
pub fn set_kernel_stack(top: u64) {
    let tss = TSS.0.get();
    // SAFETY: `tss` points at the static TSS and `top` is the top of a live
    // kernel stack. The caller runs with interrupts disabled, so no transition
    // from ring 3 can read a half written value.
    unsafe {
        core::ptr::write_unaligned(addr_of_mut!((*tss).rsp0), top);
    }
}

/// Install the GDT, initialise the TSS and load the task register.
pub fn init() {
    let tss = TSS.0.get();
    let stack = DOUBLE_FAULT_STACK.0.get();
    let interrupt_stack = INTERRUPT_STACK.0.get();

    // SAFETY: `tss`, `stack` and `interrupt_stack` point at static storage
    // owned by this module and nothing else accesses it during boot. The TSS
    // starts zeroed, so only the non-zero fields need an unaligned store.
    unsafe {
        let ist_top = addr_of!(*stack) as u64 + size_of::<DoubleFaultStack>() as u64;
        let rsp0 = addr_of!(*interrupt_stack) as u64 + size_of::<InterruptStack>() as u64;
        core::ptr::write_unaligned(addr_of_mut!((*tss).rsp0), rsp0);
        core::ptr::write_unaligned(addr_of_mut!((*tss).ist[0]), ist_top);
        core::ptr::write_unaligned(addr_of_mut!((*tss).iomap_base), size_of::<Tss>() as u16);
    }

    let tss_base = tss as u64;
    MADRUST_TSS_ADDRESS.store(tss_base, Ordering::Release);
    let (tss_low, tss_high) = tss_descriptor(tss_base);

    let gdt = GDT.0.get();
    // SAFETY: `gdt` points at the static GDT above; entries 6 and 7 are the two
    // halves of the TSS descriptor and are not read concurrently.
    unsafe {
        (*gdt).entries[6] = tss_low;
        (*gdt).entries[7] = tss_high;
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

    // SAFETY: selector 0x30 was just installed as a present, ring-0, available
    // 64-bit TSS descriptor whose base points at the initialised TSS.
    unsafe {
        asm!("ltr ax", in("ax") TSS_SELECTOR, options(nostack));
    }
}
