// SPDX-License-Identifier: GPL-2.0-only
//! Global descriptor table, task state segments and kernel stacks.
//!
//! The table keeps the flat kernel code and data descriptors loaded by the
//! 32-bit trampoline, adds the two ring 3 descriptors, and ends with one 64-bit
//! TSS descriptor per processor. Each processor loads the same table and puts
//! its own TSS selector in the task register, so `rsp0` and the IST1 stack are
//! private to that processor.
//!
//! `rsp0` points at the kernel stack of the thread that is running: the
//! scheduler writes it on every switch, exactly as the single processor kernel
//! did. A user program therefore always faults and system calls onto the stack
//! of its own thread.
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
//! | `0x30`   | TSS of processor 0 |
//! | `0x40`   | TSS of processor 1, and so on every `0x10` |
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
use core::sync::atomic::Ordering;

use crate::percpu::{self, MAX_CPUS};

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
/// First TSS descriptor selector.
const TSS_SELECTOR_BASE: u16 = 0x30;

/// IST index used by the double fault gate.
pub const DOUBLE_FAULT_IST: u8 = 1;

// The selectors `SYSRET` derives from `IA32_STAR` must match the table above:
// the instruction returns to code `STAR[63:48] + 16` and stack
// `STAR[63:48] + 8`, both with RPL 3, and `init` sets `STAR[63:48]` to
// `USER_CODE_SELECTOR`. A layout that breaks this would fault on the first
// system call return, so the relation is checked at compile time.
const _: () = assert!(SYSRET_CODE_SELECTOR == USER_CODE_SELECTOR + 16);
const _: () = assert!(USER_DATA_SELECTOR == USER_CODE_SELECTOR + 8);

/// Fixed descriptors followed by two slots per processor.
const GDT_ENTRIES: usize = 6 + MAX_CPUS * 2;

#[repr(C, align(8))]
struct Gdt {
    entries: [u64; GDT_ENTRIES],
}

/// Initial table, before the TSS descriptors are filled in.
const fn initial_entries() -> [u64; GDT_ENTRIES] {
    let mut entries = [0u64; GDT_ENTRIES];
    entries[1] = 0x00AF_9A00_0000_FFFF; // 64-bit code, ring 0
    entries[2] = 0x00AF_9200_0000_FFFF; // 64-bit data, ring 0
    entries[3] = 0x00AF_FA00_0000_FFFF; // 64-bit code, ring 3
    entries[4] = 0x00AF_F200_0000_FFFF; // 64-bit data, ring 3
    entries[5] = 0x00AF_FA00_0000_FFFF; // 64-bit code, ring 3, SYSRET target
    entries
}

struct GdtCell(UnsafeCell<Gdt>);

// SAFETY: the table is written once during boot, before any other processor
// starts and before interrupts are enabled, and is only read afterwards.
unsafe impl Sync for GdtCell {}

static GDT: GdtCell = GdtCell(UnsafeCell::new(Gdt {
    entries: initial_entries(),
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

// SAFETY: a TSS is written during boot and afterwards only `rsp0` changes, by
// the scheduler of the processor that owns it, with interrupts disabled.
unsafe impl Sync for TssCell {}

static TSS: [TssCell; MAX_CPUS] = [const { TssCell(UnsafeCell::new(TSS_ZERO)) }; MAX_CPUS];

const DOUBLE_FAULT_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct DoubleFaultStack([u8; DOUBLE_FAULT_STACK_SIZE]);

struct DoubleFaultStackCell(UnsafeCell<DoubleFaultStack>);

// SAFETY: the double fault stack of a processor is only used by the CPU when
// vector 8 is delivered on IST1 of that processor. Nothing else aliases it.
unsafe impl Sync for DoubleFaultStackCell {}

static DOUBLE_FAULT_STACK: [DoubleFaultStackCell; MAX_CPUS] = [const {
    DoubleFaultStackCell(UnsafeCell::new(DoubleFaultStack(
        [0; DOUBLE_FAULT_STACK_SIZE],
    )))
}; MAX_CPUS];

const INTERRUPT_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct InterruptStack([u8; INTERRUPT_STACK_SIZE]);

struct InterruptStackCell(UnsafeCell<InterruptStack>);

// SAFETY: the stack is the `rsp0` target of one processor, so only the CPU
// touches it, and only after a transition from ring 3.
unsafe impl Sync for InterruptStackCell {}

static INTERRUPT_STACK: [InterruptStackCell; MAX_CPUS] =
    [const { InterruptStackCell(UnsafeCell::new(InterruptStack([0; INTERRUPT_STACK_SIZE]))) };
        MAX_CPUS];

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

/// Address one past the last byte of the static ring 0 stack of `cpu`.
pub fn interrupt_stack_top(cpu: usize) -> u64 {
    let Some(cell) = INTERRUPT_STACK.get(cpu) else {
        return 0;
    };
    addr_of!(*cell.0.get()) as u64 + size_of::<InterruptStack>() as u64
}

/// Address one past the last byte of the double fault stack of `cpu`.
fn double_fault_stack_top(cpu: usize) -> u64 {
    let Some(cell) = DOUBLE_FAULT_STACK.get(cpu) else {
        return 0;
    };
    addr_of!(*cell.0.get()) as u64 + size_of::<DoubleFaultStack>() as u64
}

/// Selector of the TSS descriptor of `cpu`.
const fn tss_selector(cpu: usize) -> u16 {
    TSS_SELECTOR_BASE + (cpu as u16) * 0x10
}

/// Point `rsp0` at the kernel stack of the thread that is about to run.
///
/// The CPU reads this field on every transition from ring 3. Each processor
/// writes only its own TSS, so the field needs no lock.
pub fn set_kernel_stack(top: u64) {
    let cpu = percpu::this().index;
    let Some(cell) = TSS.get(cpu) else {
        return;
    };
    let tss = cell.0.get();
    // SAFETY: `tss` points at the static TSS of this processor and `top` is
    // the top of a live kernel stack. The caller runs with interrupts
    // disabled, so no transition from ring 3 can read a half written value.
    unsafe {
        core::ptr::write_unaligned(addr_of_mut!((*tss).rsp0), top);
    }
}

/// Address of the TSS of `cpu`, published in its per processor block.
fn tss_address(cpu: usize) -> u64 {
    TSS.get(cpu).map(|cell| cell.0.get() as u64).unwrap_or(0)
}

/// Install the GDT, initialise every TSS and load the task register.
///
/// Called on the boot processor. Application processors call [`install`] with
/// their own index; the table itself is shared and immutable after this call.
pub fn init() {
    let gdt = GDT.0.get();
    for cpu in 0..MAX_CPUS {
        let Some(cell) = TSS.get(cpu) else {
            continue;
        };
        let tss = cell.0.get();
        let ist_top = double_fault_stack_top(cpu);
        let rsp0 = interrupt_stack_top(cpu);
        // SAFETY: `tss` points at static storage owned by this module and
        // nothing else accesses it during boot. The TSS starts zeroed, so only
        // the non-zero fields need an unaligned store.
        unsafe {
            core::ptr::write_unaligned(addr_of_mut!((*tss).rsp0), rsp0);
            core::ptr::write_unaligned(addr_of_mut!((*tss).ist[0]), ist_top);
            core::ptr::write_unaligned(addr_of_mut!((*tss).iomap_base), size_of::<Tss>() as u16);
        }
        let (low, high) = tss_descriptor(tss as u64);
        // SAFETY: `gdt` points at the static table; the two slots of processor
        // `cpu` are not read while they are written.
        unsafe {
            (*gdt).entries[6 + cpu * 2] = low;
            (*gdt).entries[6 + cpu * 2 + 1] = high;
        }
    }

    load_gdt();
    load(0);
}

/// Load the shared table on the calling processor and select its own TSS.
pub fn install(cpu: usize) {
    load_gdt();
    load(cpu);
}

/// Load the descriptor table register.
fn load_gdt() {
    let gdt = GDT.0.get();
    let pointer = GdtPointer {
        limit: (size_of::<Gdt>() - 1) as u16,
        base: gdt as u64,
    };
    // SAFETY: `pointer` describes the fully populated static GDT above and
    // `lgdt` reads exactly the ten bytes of the descriptor.
    unsafe {
        asm!("lgdt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }
}

/// Reload `CS` with the ring-0 code selector of the new table.
///
/// The application processor reaches this code with the code selector of its
/// real mode trampoline, which names a different slot in this table. A far
/// return installs the kernel selector before any interrupt can save it.
fn reload_code_segment() {
    // SAFETY: selector 0x08 is the ring-0 code descriptor just installed by
    // `lgdt`, and the return address is the next instruction of this function.
    unsafe {
        asm!(
            "push {selector}",
            "lea {next}, [rip + 2f]",
            "push {next}",
            "retfq",
            "2:",
            selector = in(reg) u64::from(CODE_SELECTOR),
            next = out(reg) _,
            options(preserves_flags),
        );
    }
}

/// Load the data segments and the task register of `cpu`.
fn load(cpu: usize) {
    reload_code_segment();

    // SAFETY: selector 0x10 is the ring-0 data descriptor in the table, so
    // loading it cannot fault.
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

    // Loading `GS` resets its base, so the per processor block is installed
    // again here.
    percpu::load_base(cpu);
    let address = tss_address(cpu);
    if let Some(processor) = percpu::get(cpu) {
        processor.tss_base.store(address, Ordering::Release);
    }

    // SAFETY: the selector was just installed as a present, ring-0, available
    // 64-bit TSS descriptor whose base points at the initialised TSS.
    unsafe {
        asm!("ltr ax", in("ax") tss_selector(cpu), options(nostack));
    }
}
