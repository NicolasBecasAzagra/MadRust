// SPDX-License-Identifier: GPL-2.0-only
//! MadRust x86_64 kernel entry point (milestone M0).
//!
//! Boots through the Xen PVH protocol: QEMU jumps to `_start32`, the 32-bit
//! trampoline enables long mode and calls [`_start`].

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

mod console;
mod gdt;
mod idt;

use core::fmt::Write as _;

use madrust_arch::cpu;
use madrust_bootinfo::{BootInfo, HvmMemmapEntry, HvmStartInfo};
use madrust_drivers::debug_exit;

use crate::console::console;

core::arch::global_asm!(include_str!("trampoline.s"), options(att_syntax));
core::arch::global_asm!(include_str!("isrs.s"), options(att_syntax));

/// 64-bit kernel entry point, called by the trampoline with the physical
/// address of `hvm_start_info` in the first argument.
///
/// # Safety
///
/// Called exactly once from the `long_mode_start` assembly stub with a pointer
/// to a valid Xen PVH `hvm_start_info`. It never returns.
#[no_mangle]
unsafe extern "C" fn _start(start_info: *const HvmStartInfo) -> ! {
    let uart = console();
    uart.init();
    gdt::init();
    idt::init();

    kprintln!("MadRust: x86_64 kernel, M0");

    if start_info.is_null() {
        kprintln!("MadRust: boot FAILED: null hvm_start_info");
        debug_exit::exit(debug_exit::EXIT_BOOT_INFO_INVALID);
    }

    // SAFETY: the PVH loader guarantees a valid `hvm_start_info` at the address
    // it passed in EBX, and M0 runs with identity-mapped memory, so the
    // physical address is also a valid virtual address.
    let start = unsafe { &*start_info };

    let memmap: &[HvmMemmapEntry] = if start.memmap_entries == 0 {
        &[]
    } else if start.memmap_paddr == 0 {
        kprintln!("MadRust: boot FAILED: memmap_entries > 0 but memmap_paddr is zero");
        debug_exit::exit(debug_exit::EXIT_BOOT_INFO_INVALID);
    } else {
        // SAFETY: the PVH loader points `memmap_paddr` at `memmap_entries`
        // consecutive `hvm_memmap_table_entry` records that are identity
        // mapped and live for the whole kernel lifetime.
        unsafe {
            core::slice::from_raw_parts(
                start.memmap_paddr as *const HvmMemmapEntry,
                start.memmap_entries as usize,
            )
        }
    };

    let boot_info = match BootInfo::parse(start, memmap) {
        Ok(info) => info,
        Err(error) => {
            kprintln!("MadRust: boot FAILED: {:?}", error);
            debug_exit::exit(debug_exit::EXIT_BOOT_INFO_INVALID);
        }
    };

    kprintln!(
        "[boot] hvm_start_info at 0x{:016x}: version={} flags=0x{:x} modules={}",
        start_info as u64,
        start.version,
        start.flags,
        start.nr_modules
    );
    kprintln!("[mem] {} memory map entries", memmap.len());

    for region in boot_info.ram_regions() {
        kprintln!(
            "[mem] RAM 0x{:012x} - 0x{:012x} ({} KiB)",
            region.addr,
            region.end(),
            region.size / 1024
        );
    }

    kprintln!(
        "[mem] total usable RAM: {} bytes ({} MiB)",
        boot_info.total_ram(),
        boot_info.total_ram() / (1024 * 1024)
    );

    let cpuid = cpu::cpuid(0);
    let vendor = cpuid.vendor();
    let mut vendor_text = [0u8; 12];
    for (slot, byte) in vendor_text.iter_mut().zip(vendor.iter()) {
        *slot = *byte;
    }
    kprintln!(
        "[cpu] vendor: {}",
        core::str::from_utf8(&vendor_text).unwrap_or("unknown")
    );

    kprintln!("MadRust: boot OK");

    uart.flush();
    debug_exit::exit(debug_exit::EXIT_SUCCESS);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut uart = console();
    let _ = write!(uart, "\r\nMadRust: PANIC: {}\r\n", info);
    uart.flush();
    cpu::halt_loop()
}
