// SPDX-License-Identifier: GPL-2.0-only
//! Interrupt descriptor table with a catch-all exception handler.

use core::arch::asm;
use core::cell::UnsafeCell;

use madrust_arch::cpu;

const IDT_ENTRIES: usize = 256;
const IDT_ENTRY_SIZE: usize = 16;
const PRESENT_RING0_INTERRUPT_GATE: u8 = 0x8E;

use crate::gdt::CODE_SELECTOR;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const ZERO: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_mid: 0,
        offset_high: 0,
        reserved: 0,
    };

    fn new(handler: u64) -> Self {
        Self {
            offset_low: handler as u16,
            selector: CODE_SELECTOR,
            ist: 0,
            attributes: PRESENT_RING0_INTERRUPT_GATE,
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

struct Idt(UnsafeCell<[IdtEntry; IDT_ENTRIES]>);

// SAFETY: the table is written once during single-threaded early boot, before
// any interrupt can be delivered, and is only read by the CPU afterwards.
unsafe impl Sync for Idt {}

static IDT: Idt = Idt(UnsafeCell::new([IdtEntry::ZERO; IDT_ENTRIES]));

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

extern "C" {
    /// 256 exception stubs emitted by `isrs.s`, indexed by vector.
    static isr_table: [u64; IDT_ENTRIES];
}

/// Install the IDT with one stub per vector.
pub fn init() {
    // SAFETY: `isr_table` is defined in the linked `isrs.s` object with exactly
    // 256 eight-byte entries, one per exception vector.
    let handlers: &[u64; IDT_ENTRIES] = unsafe { &isr_table };

    // SAFETY: the IDT is a static array of IDT_ENTRIES entries and every index
    // below is smaller than that length.
    let table = unsafe { &mut *IDT.0.get() };
    for (vector, handler) in handlers.iter().enumerate() {
        table[vector] = IdtEntry::new(*handler);
    }

    let pointer = IdtPointer {
        limit: (IDT_ENTRIES * IDT_ENTRY_SIZE - 1) as u16,
        base: table.as_ptr() as u64,
    };

    // SAFETY: `pointer` describes the fully populated static IDT above and
    // `lidt` reads exactly the ten bytes of the descriptor.
    unsafe {
        asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }
}

/// Common exception entry point called from `isr_common`.
///
/// # Safety
///
/// Must only be reached through the assembly stubs in `isrs.s`, which pass the
/// vector number and the interrupted RIP.
#[no_mangle]
pub extern "C" fn madrust_exception_handler(vector: u64, rip: u64) -> ! {
    crate::kprintln!("EXCEPTION {} RIP=0x{:016x}", vector, rip);
    crate::kprintln!("MadRust: halted");

    // Make sure the message reaches the host before stopping the CPU.
    crate::console::console().flush();
    cpu::halt_loop()
}
