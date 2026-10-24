// SPDX-License-Identifier: GPL-2.0-only
//! Global descriptor table for the 64-bit kernel.
//!
//! The 32-bit trampoline already loaded a GDT with the same descriptor layout
//! (0x08 code, 0x10 data) and far-jumped through selector 0x08, so reloading CS
//! here is unnecessary; only the data segment registers and GDTR are updated.

use core::arch::asm;
use core::mem::size_of;
use core::ptr::addr_of;

/// Selector of the 64-bit code descriptor.
pub const CODE_SELECTOR: u16 = 0x08;
/// Selector of the 64-bit data descriptor.
pub const DATA_SELECTOR: u16 = 0x10;

const GDT_ENTRIES: usize = 3;

#[repr(C, align(8))]
struct Gdt {
    entries: [u64; GDT_ENTRIES],
}

/// Flat 64-bit GDT: null, code, data.
static GDT: Gdt = Gdt {
    entries: [
        0x0000_0000_0000_0000,
        0x00AF_9A00_0000_FFFF,
        0x00AF_9200_0000_FFFF,
    ],
};

#[repr(C, packed)]
struct GdtPointer {
    limit: u16,
    base: u64,
}

/// Load the kernel GDT and reload the data segment registers.
pub fn init() {
    let pointer = GdtPointer {
        limit: (size_of::<Gdt>() - 1) as u16,
        base: addr_of!(GDT) as u64,
    };

    // SAFETY: GDT is a static, 8-byte aligned table of three valid descriptors
    // and `pointer` describes it. `lgdt` reads exactly those ten bytes.
    unsafe {
        asm!("lgdt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
    }

    // SAFETY: selector 0x10 is the ring-0 data descriptor in both the
    // trampoline GDT and this one, so loading it cannot fault.
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
}
