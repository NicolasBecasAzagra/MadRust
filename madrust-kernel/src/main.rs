// SPDX-License-Identifier: GPL-2.0-only
//! MadRust x86_64 kernel entry point (milestone M2b).
//!
//! Boots through the Xen PVH protocol: QEMU jumps to `_start32`, the 32-bit
//! trampoline enables long mode with an identity map of the first GiB, and the
//! 64-bit switch routine then installs the final page tables, loads the new
//! PML4 and jumps to the higher half. From that point on the kernel runs at
//! `0xffffffff80000000` and reaches physical memory only through the direct map
//! at `0xffff888000000000`.
//!
//! The kernel then builds a physical frame allocator and a kernel heap from the
//! PVH memory map, installs a GDT with a TSS, an IDT with recoverable exception
//! stubs, remaps the 8259 PIC and programs the PIT to run at 100 Hz before
//! enabling interrupts.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
#![feature(alloc_error_handler)]

extern crate alloc;

mod console;
mod fault;
mod gdt;
mod idt;
mod memory;
mod pic;
mod pit;

use core::fmt::Write as _;

use madrust_arch::cpu;
use madrust_bootinfo::{BootInfo, HvmMemmapEntry, HvmStartInfo};
use madrust_drivers::debug_exit;
use madrust_mm::{phys_to_virt, DIRECT_MAP_BASE};

use crate::console::console;

core::arch::global_asm!(include_str!("trampoline.s"), options(att_syntax));
core::arch::global_asm!(include_str!("isrs.s"), options(att_syntax));

/// 64-bit kernel entry point, called by `higher_half_start` once the kernel
/// runs from the higher half.
///
/// `start_info_physical` is the *physical* address of `hvm_start_info`, `probe`
/// the physical address the trampoline read before the switch and `observed`
/// the value it read there.
///
/// # Safety
///
/// Called exactly once from the `higher_half_start` assembly stub with a
/// physical address that points to a valid Xen PVH `hvm_start_info`, and with a
/// probe pair taken from RAM. It never returns.
#[no_mangle]
unsafe extern "C" fn _start(start_info_physical: u64, probe: u64, observed: u64) -> ! {
    let uart = console();
    uart.init();

    kprintln!("MadRust: x86_64 kernel, M2b");
    kprintln!(
        "[mm] kernel virtual: 0x{:016x}",
        memory::kernel_virtual_base()
    );
    kprintln!("[mm] direct map: 0x{:016x}", DIRECT_MAP_BASE);

    if !memory::direct_map_probe_matches(probe, observed) {
        kprintln!("MadRust: boot FAILED: the direct map does not translate physical memory");
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_MEMORY_FAILED);
    }
    kprintln!("MM DIRECTMAP OK");

    if start_info_physical == 0 {
        kprintln!("MadRust: boot FAILED: null hvm_start_info");
        debug_exit::exit(debug_exit::EXIT_BOOT_INFO_INVALID);
    }

    // SAFETY: the PVH loader guarantees a valid `hvm_start_info` at the physical
    // address it passed in EBX, and the trampoline hands that same address
    // over. The direct map translates it into a valid virtual address.
    let start = unsafe { &*(phys_to_virt(start_info_physical) as *const HvmStartInfo) };

    let memmap: &[HvmMemmapEntry] = if start.memmap_entries == 0 {
        &[]
    } else if start.memmap_paddr == 0 {
        kprintln!("MadRust: boot FAILED: memmap_entries > 0 but memmap_paddr is zero");
        debug_exit::exit(debug_exit::EXIT_BOOT_INFO_INVALID);
    } else {
        // SAFETY: the PVH loader points `memmap_paddr` at `memmap_entries`
        // consecutive `hvm_memmap_table_entry` records in RAM, and the direct
        // map translates the table for as long as the kernel runs.
        unsafe {
            core::slice::from_raw_parts(
                phys_to_virt(start.memmap_paddr) as *const HvmMemmapEntry,
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
        "[boot] hvm_start_info at physical 0x{:016x}: version={} flags=0x{:x} modules={}",
        start_info_physical,
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

    if let Err(error) = memory::init(&boot_info) {
        kprintln!("MadRust: boot FAILED: memory manager: {:?}", error);
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_MEMORY_FAILED);
    }

    let managed_frames = memory::with(|manager| manager.managed_frames()).unwrap_or(0);
    let free_frames = memory::with(|manager| manager.free_frames()).unwrap_or(0);
    kprintln!("MEM managed frames: {}", managed_frames);
    kprintln!("MEM free frames: {}", free_frames);

    if let Err(error) = memory::self_test() {
        kprintln!("MadRust: memory self test FAILED: {:?}", error);
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_MEMORY_FAILED);
    }
    kprintln!("MEM OK");
    kprintln!("HEAP OK {}", memory::SELF_TEST_BLOCKS);

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

    gdt::init();
    kprintln!("[arch] GDT and TSS loaded");
    idt::init();
    kprintln!("[arch] IDT loaded");
    pic::init();
    kprintln!("[irq] PIC remapped, IRQ0 unmasked");
    pit::init();
    kprintln!("[irq] PIT programmed at {} Hz", pit::TICK_HZ);

    fault::trigger_divide_by_zero();
    kprintln!("MadRust: exception survived");

    cpu::enable_interrupts();
    while pit::ticks() < pit::TICK_HZ {
        cpu::hlt();
    }

    kprintln!("TICKS 100");
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
