// SPDX-License-Identifier: GPL-2.0-only
//! MadRust x86_64 kernel entry point (milestone M4b).
//!
//! Boots through the Xen PVH protocol: QEMU jumps to `_start32`, the 32-bit
//! trampoline enables long mode with an identity map of the first GiB, and the
//! 64-bit switch routine then installs the final page tables, loads the new
//! PML4 and jumps to the higher half. From that point on the kernel runs at
//! `0xffffffff80000000` and reaches physical memory only through the direct map
//! at `0xffff888000000000`.
//!
//! The kernel then builds a physical frame allocator and a kernel heap from the
//! PVH memory map, installs a GDT with one TSS per processor, an IDT with
//! recoverable exception stubs, remaps the 8259 PIC and programs the PIT. The
//! RSDP from the handover leads to the MADT, which names the processors: the
//! boot processor starts the others with INIT, SIPI, SIPI, each one gets its
//! own kernel stack, TSS, `GS` base and per processor block, and every
//! processor runs its own copy of the scheduler with its own local APIC timer.
//! The input output APIC takes the PIT line from the 8259, which stays for a
//! machine without a local APIC. Finally it spawns three kernel threads, lets
//! the timer preempt them round robin, waits for them to finish and only then
//! runs the exception and tick checks. Before the user program starts, the
//! scheduler self test measures the wakeup latency and the slice tail on the
//! target, and the processors report their heartbeats.
//!
//! After the thread demonstration the kernel loads the embedded user program
//! into the user half of the same page tables, enters ring 3 with `iretq`,
//! serves its system calls and continues with the exception and tick checks
//! once the program exits.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
#![feature(alloc_error_handler)]

extern crate alloc;

mod acpi;
mod clock;
mod console;
mod fault;
mod gdt;
mod idt;
mod ioapic;
mod lapic;
mod memory;
mod pci;
mod percpu;
mod pic;
mod pit;
mod sched;
mod sched_test;
mod smp;
mod sync;
mod thread;
mod user;
mod vectors;
mod virtio;

use core::fmt::Write as _;
use core::sync::atomic::{AtomicUsize, Ordering};

use madrust_arch::cpu;
use madrust_bootinfo::{BootInfo, HvmMemmapEntry, HvmModule, HvmStartInfo};
use madrust_drivers::debug_exit;
use madrust_mm::{phys_to_virt, DIRECT_MAP_BASE, DIRECT_MAP_SPAN};

use crate::console::console;

core::arch::global_asm!(include_str!("trampoline.s"), options(att_syntax));
core::arch::global_asm!(include_str!("isrs.s"), options(att_syntax));
core::arch::global_asm!(include_str!("context.s"), options(att_syntax));
core::arch::global_asm!(include_str!("ap_trampoline.s"), options(att_syntax));

/// Lines each demo thread prints before it returns.
const DEMO_ITERATIONS: u64 = 20;
/// Demo threads the boot path spawns.
const DEMO_THREADS: usize = 3;
/// Upper bound on the wait for the demo threads, in timer ticks (20 s).
///
/// The worst case is twenty lines per thread at one tick per line, so the
/// bound leaves a large margin while still turning a lost thread or a broken
/// scheduler into a clear failure instead of a hang.
const SCHED_DEADLINE_TICKS: u64 = 2_000;

/// Demo threads that returned from their loop.
static DEMO_DONE: AtomicUsize = AtomicUsize::new(0);

/// 64-bit kernel entry point, called by `higher_half_start` once the kernel
/// runs from the higher half.
///
/// `start_info_physical` is the *physical* address of `hvm_start_info`, `probe`
/// the physical address the trampoline read before the switch, `observed` the
/// value it read there and `highest_ram` the highest RAM end the PVH memory map
/// reported, before the trampoline limited the direct map to the page
/// directories it reserved.
///
/// # Safety
///
/// Called exactly once from the `higher_half_start` assembly stub with a
/// physical address that points to a valid Xen PVH `hvm_start_info`, and with a
/// probe pair taken from RAM. It never returns.
#[no_mangle]
unsafe extern "C" fn _start(
    start_info_physical: u64,
    probe: u64,
    observed: u64,
    highest_ram: u64,
) -> ! {
    let uart = console();
    uart.init();

    // Install the GS base of the boot processor before the first lock is
    // taken. The GDT load later resets it and calls back into `percpu`.
    percpu::init_bsp();

    kprintln!("MadRust: x86_64 kernel, M4b (SMP)");
    kprintln!(
        "[mm] kernel virtual: 0x{:016x}",
        memory::kernel_virtual_base()
    );
    kprintln!("[mm] direct map: 0x{:016x}", DIRECT_MAP_BASE);

    // The trampoline maps the physical memory the boot information reports, up
    // to the page directories it reserved. More RAM than that has no direct map
    // entry, so refuse the guest before any frame is handed out.
    if highest_ram > DIRECT_MAP_SPAN {
        kprintln!(
            "MadRust: boot FAILED: the guest has {} MiB of physical memory, the direct map describes at most {} MiB",
            highest_ram / (1024 * 1024),
            DIRECT_MAP_SPAN / (1024 * 1024)
        );
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_MEMORY_FAILED);
    }

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

    let modules: &[HvmModule] = if start.nr_modules == 0 {
        &[]
    } else if start.modlist_paddr == 0 {
        kprintln!("MadRust: boot FAILED: nr_modules > 0 but modlist_paddr is zero");
        debug_exit::exit(debug_exit::EXIT_BOOT_INFO_INVALID);
    } else {
        // SAFETY: the PVH loader points `modlist_paddr` at `nr_modules`
        // consecutive `hvm_modlist_entry` records in RAM, and the direct map
        // translates the list for as long as the kernel runs.
        unsafe {
            core::slice::from_raw_parts(
                phys_to_virt(start.modlist_paddr) as *const HvmModule,
                start.nr_modules as usize,
            )
        }
    };

    let boot_info = match BootInfo::parse(start, memmap, modules) {
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
        kprintln!("MadRust: boot FAILED: memory manager: {}", error);
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_MEMORY_FAILED);
    }

    let managed_frames = memory::with(|manager| manager.managed_frames()).unwrap_or(0);
    let free_frames = memory::with(|manager| manager.free_frames()).unwrap_or(0);
    kprintln!(
        "MEM describable: {} MiB ({} frames)",
        DIRECT_MAP_SPAN / (1024 * 1024),
        memory::MAX_FRAMES
    );
    kprintln!(
        "MEM managed: {} MiB",
        managed_frames as u64 * madrust_mm::FRAME_SIZE as u64 / (1024 * 1024)
    );
    kprintln!("MEM metadata: {} bytes (1 byte per frame)", managed_frames);
    kprintln!("MEM managed frames: {}", managed_frames);
    kprintln!("MEM free frames: {}", free_frames);

    if let Err(error) = memory::self_test() {
        kprintln!("MadRust: memory self test FAILED: {:?}", error);
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_MEMORY_FAILED);
    }
    kprintln!("MEM OK");
    kprintln!("HEAP OK {}", memory::SELF_TEST_BLOCKS);

    // Parse the program and build its address space and stack now, so a bad
    // image stops the boot before the threads start. An initramfs handed over
    // as a PVH module wins over the embedded copy; without one the embedded
    // program keeps the old path, including its boot log.
    let program = match user::initramfs::find(boot_info.modules().len(), boot_info.modules()) {
        Some(initramfs) => {
            let modules = initramfs.module_count();
            let files = initramfs.file_count();
            let entries = initramfs.entry_count();
            kprintln!(
                "[user] initramfs: {} {}, {} {}, {} {}",
                modules,
                if modules == 1 { "module" } else { "modules" },
                files,
                if files == 1 { "file" } else { "files" },
                entries,
                if entries == 1 { "entry" } else { "entries" }
            );
            user::initramfs::install(&initramfs);
            let Some(file) = initramfs.program() else {
                kprintln!("MadRust: boot FAILED: initramfs has no executable");
                uart.flush();
                debug_exit::exit(debug_exit::EXIT_USER_FAILED);
            };
            kprintln!("[user] program: /{} ({} bytes)", file.name, file.data.len());
            match user::load_program(file.data) {
                Ok(program) => program,
                Err(error) => {
                    kprintln!("MadRust: boot FAILED: user program: {:?}", error);
                    uart.flush();
                    debug_exit::exit(debug_exit::EXIT_USER_FAILED);
                }
            }
        }
        None => {
            kprintln!("[user] embedded ELF: {} bytes", user::embedded_size());
            match user::load() {
                Ok(program) => program,
                Err(error) => {
                    kprintln!("MadRust: boot FAILED: user program: {:?}", error);
                    uart.flush();
                    debug_exit::exit(debug_exit::EXIT_USER_FAILED);
                }
            }
        }
    };
    // The image is loaded at a fixed base. A position independent executable
    // carries an entry offset, so the first instruction is base + e_entry; both
    // numbers are logged to keep a wrong base visible.
    kprintln!("[user] entry at 0x{:x}", user::load_base(&program));
    kprintln!(
        "[user] start at 0x{:x} (base + e_entry)",
        user::entry(&program)
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

    // The configuration space is reached through two ports and needs no
    // interrupt, so the bus is enumerated before the interrupt controller is
    // reprogrammed.
    pci::enumerate();

    // Take the network card when the machine carries one. The legacy window
    // is polled, so this stays before the interrupt controller is programmed
    // too; a card that does not answer leaves the rest of the boot alone.
    if let Err(error) = virtio::probe() {
        kprintln!("[virtio] net: {:?}", error);
    }

    gdt::init();
    kprintln!("[arch] GDT and TSS loaded");
    idt::init();
    kprintln!("[arch] IDT loaded");
    pic::init();
    kprintln!("[irq] PIC remapped, IRQ0 unmasked");
    pit::init();
    kprintln!(
        "[irq] PIT programmed at {} Hz ({} Hz accounting tick)",
        pit::IRQ_HZ,
        pit::TICK_HZ
    );
    let tsc_hz = clock::init();
    kprintln!("[time] TSC {} MHz", tsc_hz / 1_000_000);

    sched::init();

    // Discover the processors and start them before the workloads exist. The
    // boot processor keeps the loaded thread as its current context; every
    // other one enters an idle loop on its own stack.
    let online = smp::init(start.rsdp_paddr);
    if online > 1 {
        match smp::wake_test() {
            Some(target) => kprintln!("SMP wake cpu={} OK", target),
            None => {
                kprintln!("MadRust: boot FAILED: cannot wake a thread on another processor");
                uart.flush();
                debug_exit::exit(debug_exit::EXIT_SCHEDULER_FAILED);
            }
        }
    }
    let mut demo_ids = [usize::MAX; DEMO_THREADS];
    for (slot, name) in ["T1", "T2", "T3"].into_iter().enumerate() {
        match sched::spawn(name, demo_thread) {
            Ok(id) => demo_ids[slot] = id,
            Err(error) => {
                kprintln!(
                    "MadRust: boot FAILED: cannot create thread {}: {:?}",
                    name,
                    error
                );
                uart.flush();
                debug_exit::exit(debug_exit::EXIT_SCHEDULER_FAILED);
            }
        }
    }

    cpu::enable_interrupts();

    // The first timer tick preempts this thread and the three demo threads run
    // round robin. Count the demo threads themselves: the scheduler counter
    // also includes the probe thread that ran on another processor.
    let deadline = pit::ticks() + SCHED_DEADLINE_TICKS;
    while DEMO_DONE.load(Ordering::Acquire) < DEMO_THREADS {
        if pit::ticks() >= deadline {
            kprintln!(
                "MadRust: boot FAILED: scheduler stopped with {} of {} demo threads finished",
                DEMO_DONE.load(Ordering::Acquire),
                DEMO_THREADS
            );
            for id in demo_ids {
                kprintln!(
                    "MadRust: lost demo thread {} state {:?} queue {:?}",
                    id,
                    thread::state(id),
                    sched::queue_of(id)
                );
            }
            uart.flush();
            debug_exit::exit(debug_exit::EXIT_SCHEDULER_FAILED);
        }
        sched::yield_now();
        cpu::hlt();
    }

    let reclaimed = thread::reclaim_finished();
    let mut lost = 0;
    for id in demo_ids {
        if let Some(state) = thread::state(id) {
            lost += 1;
            kprintln!(
                "MadRust: lost demo thread {} state {:?} cpu {:?} on_cpu {:?} queue {:?}",
                id,
                state,
                thread::cpu(id),
                thread::on_cpu(id),
                sched::queue_of(id)
            );
        }
    }
    if lost != 0 {
        kprintln!(
            "MadRust: boot FAILED: scheduler lost {} demo threads ({} slots reclaimed)",
            lost,
            reclaimed
        );
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_SCHEDULER_FAILED);
    }

    // Measure the scheduler on the target before the program takes over: a
    // wakeup that waits for the tick would show up here as a tail of a full
    // slice, and the test stops the boot instead of reporting a healthy run.
    if !sched_test::run() {
        kprintln!("MadRust: boot FAILED: scheduler self test");
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_SCHEDULER_FAILED);
    }

    // Run the user program. It prints through the write system call and leaves
    // through exit_group; the kernel prints the status it received.
    let status = user::run(&program);
    kprintln!("USER OK exit={}", status);
    if status != 0 {
        kprintln!(
            "MadRust: boot FAILED: user program exited with status {}",
            status
        );
        uart.flush();
        debug_exit::exit(debug_exit::EXIT_USER_FAILED);
    }
    kprintln!("SCHED OK");

    fault::trigger_divide_by_zero();
    kprintln!("MadRust: exception survived");

    while pit::ticks() < pit::TICK_HZ {
        cpu::hlt();
    }

    kprintln!("TICKS 100");
    smp::report_heartbeats();
    kprintln!("SMP OK cpus={}", smp::online_count());
    kprintln!("MadRust: boot OK");

    uart.flush();
    debug_exit::exit(debug_exit::EXIT_SUCCESS);
}

/// Entry function of every demo thread.
///
/// The thread control block supplies the name, so one function serves all
/// three threads. The `hlt` between lines waits for the next timer tick; with
/// one-tick time slices every thread prints exactly one line per turn, so the
/// serial log alternates between the three identifiers.
fn demo_thread(name: &'static str) {
    for iteration in 0..DEMO_ITERATIONS {
        kprintln!("{name} {iteration}");
        cpu::hlt();
    }
    DEMO_DONE.fetch_add(1, Ordering::Release);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut uart = console();
    let _ = write!(uart, "\r\nMadRust: PANIC: {}\r\n", info);
    uart.flush();
    cpu::halt_loop()
}
