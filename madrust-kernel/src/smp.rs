// SPDX-License-Identifier: GPL-2.0-only
//! Discovery and startup of the other processors (milestone E3b).
//!
//! ## Discovery
//!
//! The RSDP comes from the PVH handover. [`crate::acpi`] validates it and walks
//! the RSDT or XSDT to the MADT, which names every enabled local APIC, the
//! local APIC base and the input output APIC. A machine without that
//! description is not an error: the kernel keeps the PIT and the 8259 and
//! prints why it stayed on one processor.
//!
//! ## Startup
//!
//! The boot processor copies [`ap_trampoline`](crate::ap_trampoline) to a page
//! below 1 MiB, temporarily installs the identity map of the first GiB that the
//! early boot code left in `.bss`, and sends INIT, SIPI, SIPI to one processor
//! at a time. The trampoline enters long mode on the kernel page tables and
//! calls [`ap_main`], which installs the GDT, the IDT and the local APIC timer
//! of that processor and then enters its idle loop. Once every processor is
//! online the identity map is removed and a TLB shootdown makes every processor
//! drop it.
//!
//! ## Verification
//!
//! A processor is online only after its timer has interrupted it at least once,
//! so a processor that failed the startup sequence is reported instead of
//! sitting silently in the idle loop.

use core::ptr::addr_of;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use madrust_arch::cpu;
use madrust_mm::phys_to_virt;

use crate::acpi;
use crate::clock;
use crate::gdt;
use crate::idt;
use crate::ioapic;
use crate::lapic;
use crate::memory;
use crate::percpu::{self, MAX_CPUS};
use crate::sched;
use crate::thread;
use crate::vectors::{IPI_TLB_VECTOR, PIT_IOAPIC_VECTOR};

/// Physical address the trampoline is copied to.
const AP_TRAMPOLINE_PHYS: u64 = 0x8000;
/// Real mode start page of the trampoline, the SIPI operand.
const AP_TRAMPOLINE_PAGE: u8 = 0x08;
/// Physical load address of the kernel image.
const KERNEL_PHYSICAL_BASE: u64 = 0x100_000;
/// Delay between INIT and the first SIPI, in microseconds.
const INIT_DELAY_US: u64 = 10_000;
/// Delay between the two SIPIs, in microseconds.
const SIPI_DELAY_US: u64 = 200;
/// Time to wait for one processor to report itself online, in milliseconds.
const ONLINE_TIMEOUT_MS: u64 = 500;
/// Time to wait for the first timer interrupt of an online processor.
const HEARTBEAT_TIMEOUT_MS: u64 = 100;

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static ap_trampoline_cr3: u8;
    static ap_trampoline_stack: u8;
    static ap_trampoline_percpu: u8;
    static ap_trampoline_index: u8;
    static ap_trampoline_entry: u8;
    static boot_pdpt: u8;
    static mm_pml4: u8;
}

/// Number of processors that passed the heartbeat check.
static ONLINE: AtomicUsize = AtomicUsize::new(1);
/// Kernel stack top of every processor, for the startup sequence.
static STACKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Number of processors that are running.
pub fn online_count() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// Find and start the other processors.
///
/// Returns the number of processors that are online, including the boot
/// processor. Called once on the boot processor with interrupts disabled.
pub fn init(rsdp_paddr: u64) -> usize {
    let boot_apic = boot_apic_id();
    let Some(processor) = percpu::get(0) else {
        return 1;
    };
    processor.apic_id.store(boot_apic, Ordering::Release);

    let info = match acpi::discover(rsdp_paddr) {
        Ok(info) => info,
        Err(error) => {
            crate::kprintln!(
                "SMP processors=1 boot=0: no usable ACPI tables ({:?})",
                error
            );
            return 1;
        }
    };
    let Some(ioapic_info) = info.ioapic else {
        crate::kprintln!("SMP processors=1 boot=0: the MADT names no input output APIC");
        return 1;
    };
    if info.processor_count == 0 || !contains(&info, boot_apic) {
        crate::kprintln!(
            "SMP processors=1 boot=0: the MADT does not describe the boot processor (APIC {})",
            boot_apic
        );
        return 1;
    }

    // The boot processor is index zero; the other enabled APICS follow in
    // table order. A table that lists the boot processor later is reordered so
    // that every index in the kernel has one meaning.
    let mut apic_ids = [0u32; MAX_CPUS];
    apic_ids[0] = boot_apic;
    let mut count = 1;
    for apic_id in info.processors[..info.processor_count].iter().copied() {
        if apic_id == boot_apic || count >= MAX_CPUS {
            continue;
        }
        apic_ids[count] = apic_id;
        count += 1;
    }
    if info.described_processors > MAX_CPUS {
        crate::kprintln!(
            "SMP: {} processors described, the kernel starts at most {}",
            info.described_processors,
            MAX_CPUS
        );
    }

    for index in 1..count {
        let Some(block) = percpu::allocate(index) else {
            crate::kprintln!("SMP cpu {}: cannot allocate its per processor block", index);
            return online_count();
        };
        block.apic_id.store(apic_ids[index], Ordering::Release);
        let Some(block_allocation) = memory::with(|manager| manager.alloc(thread::STACK_SIZE))
        else {
            crate::kprintln!("SMP cpu {}: cannot allocate its kernel stack", index);
            return online_count();
        };
        let Some(stack) = block_allocation else {
            crate::kprintln!("SMP cpu {}: cannot allocate its kernel stack", index);
            return online_count();
        };
        let top = stack.as_ptr() as u64 + thread::STACK_SIZE as u64;
        STACKS[index].store(top, Ordering::Release);
        thread::install_idle(index, "idle", top);
        block.idle_thread.store(index, Ordering::Release);
    }

    let lapic_address = if info.lapic_address == 0 {
        0xfee0_0000
    } else {
        info.lapic_address
    };
    if !memory::map_mmio(lapic_address) || !memory::map_mmio(ioapic_info.address) {
        crate::kprintln!("SMP processors=1 boot=0: the APIC windows are outside the direct map");
        return 1;
    }

    lapic::init();
    let calibrated = lapic::calibrate();
    if calibrated == 0 {
        crate::kprintln!("SMP processors=1 boot=0: the local APIC timer did not calibrate");
        return 1;
    }
    if !lapic::start_timer() {
        crate::kprintln!("SMP processors=1 boot=0: the local APIC timer did not start");
        return 1;
    }
    percpu::this().heartbeat.store(0, Ordering::Release);

    if !ioapic::init(ioapic_info.address, ioapic_info.gsi_base) {
        crate::kprintln!(
            "SMP processors=1 boot=0: the input output APIC at 0x{:x} is unusable",
            ioapic_info.address
        );
        return 1;
    }
    let (level, low) = ioapic::mps_flags(info.timer_flags);
    ioapic::route(info.timer_gsi, PIT_IOAPIC_VECTOR, boot_apic, level, low);
    crate::pic::mask_all();
    crate::kprintln!(
        "[acpi] MADT: {} processors ({} described), local APIC 0x{:x}, input output APIC 0x{:x}, timer GSI {}",
        count,
        info.described_processors,
        info.lapic_address,
        ioapic_info.address,
        info.timer_gsi
    );

    if count == 1 {
        ONLINE.store(1, Ordering::Release);
        crate::kprintln!("SMP processors=1 boot=0 (one processor in the MADT)");
        return 1;
    }

    start_application_processors(&apic_ids[..count]);
    let online = online_count();
    crate::kprintln!("SMP processors={} boot=0 online={}", count, online);
    online
}

/// Identifier of the local APIC of the boot processor.
///
/// The register window is read directly instead of the CPUID leaf: the leaf
/// only describes the initial identifier and the firmware already mapped the
/// controller for us. The window is installed in the direct map first.
fn boot_apic_id() -> u32 {
    const MSR_APIC_BASE: u32 = 0x1b;
    const DEFAULT_APIC_BASE: u64 = 0xfee0_0000;
    const REG_ID: u64 = 0x20;
    let configured = cpu::read_msr(MSR_APIC_BASE) & 0x000f_ffff_ffff_f000;
    let base = if configured == 0 {
        DEFAULT_APIC_BASE
    } else {
        configured
    };
    if !memory::map_mmio(base) {
        return 0;
    }
    // SAFETY: `map_mmio` installed the window in the direct map and the
    // identifier register accepts a 32-bit read.
    unsafe { core::ptr::read_volatile(phys_to_virt(base + REG_ID) as *const u32) >> 24 }
}

/// `true` when the firmware listed `apic_id`.
fn contains(info: &acpi::Info, apic_id: u32) -> bool {
    info.processors[..info.processor_count].contains(&apic_id)
}

/// Start every processor after the boot one.
fn start_application_processors(apic_ids: &[u32]) {
    install_trampoline();
    let saved_entry = enable_identity_map();

    let mut started = 1;
    for (index, apic_id) in apic_ids.iter().copied().enumerate().skip(1) {
        if !start_one(index, apic_id) {
            crate::kprintln!("SMP cpu {} (APIC {}) did not start", index, apic_id);
            continue;
        }
        started += 1;
    }

    disable_identity_map(saved_entry);
    ONLINE.store(started, Ordering::Release);
}

/// Copy the trampoline blob to its physical page.
fn install_trampoline() {
    let start = addr_of!(ap_trampoline_start);
    let end = addr_of!(ap_trampoline_end);
    let size = end as usize - start as usize;
    // SAFETY: both symbols bound one object in the kernel image and the
    // destination is the low page reserved by the memory manager for real mode
    // data. The copy is the only access to that page before an AP starts.
    unsafe {
        core::ptr::copy_nonoverlapping(start, phys_to_virt(AP_TRAMPOLINE_PHYS) as *mut u8, size);
    }
}

/// Temporarily map the first GiB at low addresses for the startup code.
///
/// The user program is already loaded in PML4 entry zero, so the previous value
/// is returned and restored after the last processor has left real mode.
fn enable_identity_map() -> u64 {
    let pml4 = addr_of!(mm_pml4) as *mut u64;
    let pdpt = physical(addr_of!(boot_pdpt) as u64);
    let previous = unsafe { pml4.read() };
    // SAFETY: `mm_pml4` is the top level table of the running address space and
    // entry zero holds either nothing or the user half. The memory manager
    // reserves the whole page table block with the kernel image.
    unsafe {
        pml4.write(pdpt | 0x3);
        cpu::flush_tlb();
    }
    previous
}

/// Restore the low half and make every processor drop the identity map.
fn disable_identity_map(previous: u64) {
    let pml4 = addr_of!(mm_pml4) as *mut u64;
    // SAFETY: the value was read from the same entry by `enable_identity_map`
    // and restores the user half the loader built before the startup sequence.
    unsafe {
        pml4.write(previous);
        cpu::flush_tlb();
    }
    lapic::broadcast_ipi(IPI_TLB_VECTOR);
}

/// Start one processor with INIT, SIPI, SIPI.
fn start_one(index: usize, apic_id: u32) -> bool {
    let Some(processor) = percpu::get(index) else {
        return false;
    };
    let Some(stack) = STACKS.get(index).map(|slot| slot.load(Ordering::Acquire)) else {
        return false;
    };
    if stack == 0 {
        return false;
    }
    patch(
        addr_of!(ap_trampoline_cr3),
        physical(addr_of!(mm_pml4) as u64),
    );
    patch(addr_of!(ap_trampoline_stack), stack);
    patch(addr_of!(ap_trampoline_percpu), processor.base());
    patch(addr_of!(ap_trampoline_index), index as u64);
    patch(
        addr_of!(ap_trampoline_entry),
        ap_main as *const () as usize as u64,
    );

    lapic::send_init(apic_id);
    delay_us(INIT_DELAY_US);
    lapic::send_startup(apic_id, AP_TRAMPOLINE_PAGE);
    delay_us(SIPI_DELAY_US);
    lapic::send_startup(apic_id, AP_TRAMPOLINE_PAGE);

    if !wait_online(index, ONLINE_TIMEOUT_MS) {
        return false;
    }
    if wait_heartbeat(index, HEARTBEAT_TIMEOUT_MS) {
        true
    } else {
        // The processor reached its idle loop but its timer never interrupted
        // it. Take it out of the scheduler targets instead of leaving a CPU
        // that cannot be preempted.
        processor.online.store(false, Ordering::Release);
        false
    }
}

/// Write one word of the copied trampoline.
///
/// The offset is taken from the linked symbols, so the patched word is in the
/// copy the processor executes, not in the read only image.
fn patch(symbol: *const u8, value: u64) {
    let start = addr_of!(ap_trampoline_start);
    let offset = symbol as usize - start as usize;
    let destination = phys_to_virt(AP_TRAMPOLINE_PHYS + offset as u64) as *mut u64;
    // SAFETY: `symbol` names a word inside the trampoline blob and the copy at
    // `AP_TRAMPOLINE_PHYS` is writable low memory owned by the boot path.
    unsafe {
        destination.write(value);
    }
}

/// Translate a kernel virtual address into its physical load address.
fn physical(address: u64) -> u64 {
    address - memory::kernel_virtual_base() + KERNEL_PHYSICAL_BASE
}

/// Wait until a processor reports itself online.
fn wait_online(index: usize, milliseconds: u64) -> bool {
    let deadline = milliseconds.saturating_mul(1000);
    let mut waited = 0;
    while waited < deadline {
        if percpu::get(index).is_some_and(|processor| processor.online.load(Ordering::Acquire)) {
            return true;
        }
        delay_us(100);
        waited += 100;
    }
    false
}

/// Wait until the timer of an online processor has interrupted it once.
fn wait_heartbeat(index: usize, milliseconds: u64) -> bool {
    let deadline = milliseconds.saturating_mul(1000);
    let mut waited = 0;
    while waited < deadline {
        if percpu::get(index)
            .is_some_and(|processor| processor.heartbeat.load(Ordering::Acquire) != 0)
        {
            return true;
        }
        delay_us(100);
        waited += 100;
    }
    false
}

/// Busy wait for `microseconds`, using the calibrated time stamp counter.
fn delay_us(microseconds: u64) {
    let hertz = clock::tsc_hz();
    if hertz == 0 {
        let mut spins = microseconds.saturating_mul(100);
        while spins > 0 {
            core::hint::spin_loop();
            spins -= 1;
        }
        return;
    }
    let cycles = microseconds.saturating_mul(hertz) / 1_000_000;
    let start = cpu::read_tsc();
    while cpu::read_tsc().wrapping_sub(start) < cycles {
        core::hint::spin_loop();
    }
}

/// Move one thread from the boot processor to the second processor.
///
/// The thread only returns, so the test proves that the queue push, the
/// reschedule interrupt and the idle wakeup of the target processor work. The
/// boot processor spins with interrupts disabled; the target runs on its own.
pub fn wake_test() -> Option<usize> {
    let target = (1..MAX_CPUS).find(|index| {
        percpu::get(*index).is_some_and(|processor| processor.online.load(Ordering::Acquire))
    })?;
    let before = sched::finished_threads();
    if sched::spawn_on_cpu(
        target,
        "probe",
        probe_thread,
        madrust_sched::Priority::DEFAULT,
    )
    .is_err()
    {
        return None;
    }
    let hertz = clock::tsc_hz();
    let deadline = hertz / 2;
    let start = cpu::read_tsc();
    let mut spins = 0u64;
    loop {
        if sched::finished_threads() > before {
            return Some(target);
        }
        if hertz == 0 {
            spins += 1;
            if spins > 100_000_000 {
                return None;
            }
        } else if cpu::read_tsc().wrapping_sub(start) > deadline {
            return None;
        }
        core::hint::spin_loop();
    }
}

/// Entry function of the cross processor probe.
fn probe_thread(_name: &'static str) {}

/// Print one heartbeat line with the processor that reported it.
pub fn report_heartbeats() {
    crate::kprint!("SMP heartbeat");
    for index in 0..MAX_CPUS {
        let Some(processor) = percpu::get(index) else {
            continue;
        };
        crate::kprint!(
            " cpu{}={}{}",
            index,
            processor.heartbeat.load(Ordering::Acquire),
            if processor.online.load(Ordering::Acquire) {
                ""
            } else {
                "!"
            }
        );
    }
    crate::kprintln!();
}

/// Entry point of an application processor, called by the trampoline.
///
/// It runs with interrupts disabled, on the kernel page tables and on the stack
/// the boot processor patched into the trampoline.
#[no_mangle]
pub extern "C" fn ap_main(index: u64) -> ! {
    let index = usize::try_from(index).unwrap_or(0);
    percpu::load_base(index);
    gdt::install(index);
    idt::load();
    lapic::init();
    if !lapic::start_timer() {
        crate::kprintln!("SMP cpu {}: the local APIC timer did not start", index);
        cpu::halt_loop();
    }
    crate::kprintln!("SMP cpu {} up APIC {}", index, lapic::id());
    sched::ap_start(index)
}
