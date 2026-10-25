// SPDX-License-Identifier: GPL-2.0-only
//! Interrupt descriptor table and the common interrupt handler.

use core::arch::asm;
use core::cell::UnsafeCell;

use madrust_arch::cpu;

use crate::gdt::{CODE_SELECTOR, DOUBLE_FAULT_IST};
use crate::lapic;
use crate::pic;
use crate::pit;
use crate::user::vm::PageFaultOutcome;
use crate::vectors::{
    IPI_RESCHEDULE_VECTOR, IPI_TLB_VECTOR, LAPIC_TIMER_VECTOR, PIT_IOAPIC_VECTOR, SPURIOUS_VECTOR,
};

const IDT_ENTRIES: usize = 256;
const IDT_ENTRY_SIZE: usize = 16;
const PRESENT_RING0_INTERRUPT_GATE: u8 = 0x8E;

/// Interrupted context as laid out on the stack by `isr_common`.
///
/// The order matches the pushes in `isrs.s`, lowest address first.
#[repr(C)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub error_code: u64,
    pub vector: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
}

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

    fn new(handler: u64, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: CODE_SELECTOR,
            ist,
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
    /// 256 stubs emitted by `isrs.s`, indexed by vector.
    static isr_table: [u64; IDT_ENTRIES];
}

/// Install the IDT with one stub per vector and load it here.
pub fn init() {
    build();
    load();
}

/// Fill every entry of the table once.
fn build() {
    // SAFETY: `isr_table` is defined in the linked `isrs.s` object with exactly
    // 256 eight-byte entries, one per vector.
    let handlers: &[u64; IDT_ENTRIES] = unsafe { &isr_table };

    // SAFETY: the IDT is a static array of IDT_ENTRIES entries and every index
    // below is smaller than that length.
    let table = unsafe { &mut *IDT.0.get() };
    for (vector, handler) in handlers.iter().enumerate() {
        // The double fault runs on IST1 so that a corrupt kernel stack does not
        // turn the fault into a triple fault.
        let ist = if vector == 8 { DOUBLE_FAULT_IST } else { 0 };
        table[vector] = IdtEntry::new(*handler, ist);
    }
}

/// Load the IDT on the calling processor.
///
/// The table itself is shared and immutable after [`init`], so an application
/// processor only has to install its descriptor.
pub fn load() {
    // SAFETY: the table is a static array of IDT_ENTRIES entries and every
    // index used by the build is smaller than that length.
    let table = unsafe { &*IDT.0.get() };
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

fn exception_name(vector: u64) -> &'static str {
    match vector {
        0 => "DIV0",
        6 => "INVALIDOP",
        8 => "DOUBLEFAULT",
        13 => "GP",
        14 => "PAGEFAULT",
        _ => "UNKNOWN",
    }
}

/// Common entry point called from `isr_common` for every vector.
///
/// # Safety
///
/// Must only be reached through the assembly stubs in `isrs.s`, which pass a
/// pointer to a complete [`InterruptFrame`] on the kernel stack.
#[no_mangle]
pub extern "C" fn madrust_interrupt_handler(frame: &mut InterruptFrame) {
    let vector = frame.vector;

    // The local APIC timer of this processor drives preemption. The end of
    // interrupt comes first: the switch may keep this thread off the CPU for a
    // while, and the next tick must not be lost.
    if vector == u64::from(LAPIC_TIMER_VECTOR) {
        lapic::eoi();
        crate::sched::on_tick();
        return;
    }

    // Another processor placed a thread in this processor's run queue.
    if vector == u64::from(IPI_RESCHEDULE_VECTOR) {
        lapic::eoi();
        crate::sched::on_ipi();
        return;
    }

    // The boot processor cleared the identity mapping the startup sequence
    // needed; every other processor drops its cached translations.
    if vector == u64::from(IPI_TLB_VECTOR) {
        // SAFETY: the page tables are consistent and only stale translations
        // of the removed identity map are being dropped.
        unsafe { cpu::flush_tlb() };
        lapic::eoi();
        return;
    }

    // The PIT keeps the accounting tick when the input output APIC carries its
    // line, so the 8259 range is empty on those machines.
    if vector == u64::from(PIT_IOAPIC_VECTOR) {
        pit::record_tick();
        lapic::eoi();
        return;
    }

    // A machine without a local APIC keeps the 8259 and its timer vector. When
    // the local APIC timer drives preemption, the PIT only counts ticks.
    if vector == u64::from(pic::TIMER_VECTOR) {
        pit::record_tick();
        pic::send_eoi(vector as u8);
        if !lapic::timer_active() {
            crate::sched::on_tick();
        }
        return;
    }

    if (u64::from(pic::IRQ_BASE)..u64::from(pic::IRQ_BASE) + 16).contains(&vector) {
        crate::kprintln!("IRQ vector=0x{:02x}", vector);
        pic::send_eoi(vector as u8);
        return;
    }

    if vector == u64::from(SPURIOUS_VECTOR) {
        return;
    }

    // A page fault in the user half is the demand paging path: the handler
    // installs the frame the program just touched and the instruction retries.
    if vector == 14 {
        let address = cpu::read_cr2();
        let outcome = crate::user::vm::handle_page_fault(address, frame.error_code);
        if outcome == PageFaultOutcome::Resolved {
            return;
        }
        fatal_page_fault(frame, address, outcome);
    }

    // A fault in ring 3 cannot be resumed by the kernel: report it with the
    // reason and the program counter, then leave QEMU instead of hanging.
    if frame.cs & 3 == 3 {
        crate::kprintln!(
            "MadRust: user program FAILED: exception {} {} RIP=0x{:016x}",
            vector,
            exception_name(vector),
            frame.rip
        );
        if vector == 14 {
            crate::kprintln!("PAGEFAULT ADDR=0x{:016x}", cpu::read_cr2());
        }
        crate::console::console().flush();
        crate::debug_exit::exit(crate::debug_exit::EXIT_USER_FAILED);
    }

    match vector {
        0 => {
            crate::kprintln!(
                "EXCEPTION {} {} RIP=0x{:016x}",
                vector,
                exception_name(vector),
                frame.rip
            );
            if crate::fault::resume_after_divide_by_zero(frame) {
                return;
            }
        }
        _ => {
            crate::kprintln!(
                "EXCEPTION {} {} RIP=0x{:016x} ERR=0x{:x} CS=0x{:x}",
                vector,
                exception_name(vector),
                frame.rip,
                frame.error_code,
                frame.cs
            );
        }
    }

    // An exception that cannot be recovered from stops the kernel. Returning
    // to the faulting instruction would repeat the same fault forever, so the
    // only way out is to halt with the report already on the serial line.
    crate::console::console().flush();
    cpu::halt_loop();
}

/// Report a page fault the kernel cannot resolve and stop.
///
/// A fault from ring 3 stops the user program with the faulting address from
/// CR2; a fault from the kernel is a kernel bug, because the kernel half and
/// the direct map are fully mapped. Both paths print a reason and never return.
#[cold]
fn fatal_page_fault(frame: &InterruptFrame, address: u64, outcome: PageFaultOutcome) -> ! {
    if frame.cs & 3 == 3 {
        crate::kprintln!(
            "MadRust: user program FAILED: page fault at 0x{:016x}: {} (RIP=0x{:016x})",
            address,
            outcome.describe(),
            frame.rip
        );
        crate::console::console().flush();
        crate::debug_exit::exit(crate::debug_exit::EXIT_USER_FAILED);
    }

    crate::kprintln!(
        "MadRust: KERNEL BUG: page fault at 0x{:016x}: {} (RIP=0x{:016x})",
        address,
        outcome.describe(),
        frame.rip
    );
    crate::console::console().flush();
    cpu::halt_loop();
}
