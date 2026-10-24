// SPDX-License-Identifier: GPL-2.0-only
//! QEMU `isa-debug-exit` helper.
//!
//! With `-device isa-debug-exit,iobase=0xf4,iosize=0x04`, any 32-bit write to
//! port `0xF4` makes QEMU terminate with status `(value << 1) | 1`.

use madrust_arch::port;

/// I/O port of the QEMU `isa-debug-exit` device.
pub const ISA_DEBUG_EXIT_PORT: u16 = 0xF4;

/// Exit code used by the kernel on a successful boot: QEMU exits with 33.
pub const EXIT_SUCCESS: u32 = 0x10;

/// Exit code used when boot information is invalid: QEMU exits with 35.
pub const EXIT_BOOT_INFO_INVALID: u32 = 0x11;

/// Exit code used when the memory manager cannot be built or its self test
/// fails: QEMU exits with 37.
pub const EXIT_MEMORY_FAILED: u32 = 0x12;

/// Exit code used when a thread cannot be created, the boot wait expires or
/// the scheduler loses the running thread: QEMU exits with 39.
pub const EXIT_SCHEDULER_FAILED: u32 = 0x13;

/// Terminate QEMU with `(code << 1) | 1`.
///
/// If the device is absent the write is ignored, so spin forever after it.
pub fn exit(code: u32) -> ! {
    madrust_arch::cpu::disable_interrupts();
    port::outl(ISA_DEBUG_EXIT_PORT, code);
    madrust_arch::cpu::halt_loop()
}
