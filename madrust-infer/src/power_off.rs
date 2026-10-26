// SPDX-License-Identifier: GPL-2.0-only
//! Asking the machine to go down before the last process leaves.
//!
//! Mainline Linux panics with "Attempted to kill init!" when PID 1 exits,
//! and every program of this project is PID 1 in the guest: the benchmark
//! workload, the inference runtime and the agent. Asking the kernel to power
//! the machine off ends the run without that panic and without the emulator
//! waiting for a guest that will never leave.
//!
//! The call is the Linux `reboot` system call with the Linux magic words:
//!
//! ```text
//! reboot(magic1, magic2, cmd)
//! rdi = 0x01234567   LINUX_REBOOT_MAGIC1
//! rsi = 0xCDEF0123   LINUX_REBOOT_MAGIC2
//! rdx = 0x4321FEDC   LINUX_REBOOT_CMD_POWER_OFF
//! ```
//!
//! The kernel answers `-EINVAL` when the two magic words are not in the first
//! two arguments, so the order of the arguments is not a detail.
//!
//! A kernel that does not know the call answers with an error and the caller
//! carries on, which is what MadRust does: it leaves the machine through its
//! own path.

/// The Linux `reboot` system call.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_REBOOT: u64 = 169;

/// `LINUX_REBOOT_MAGIC1`: the first word the kernel checks.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LINUX_REBOOT_MAGIC1: u64 = 0x0123_4567;

/// `LINUX_REBOOT_MAGIC2`: the second word the kernel checks.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LINUX_REBOOT_MAGIC2: u64 = 0xCDEF_0123;

/// `LINUX_REBOOT_CMD_POWER_OFF`: the command that stops the machine.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const LINUX_REBOOT_CMD_POWER_OFF: u64 = 0x4321_FEDC;

/// Ask the machine to power off.
///
/// The call never returns on a kernel that accepts it. On a kernel that
/// refuses it the result is an error number and the caller continues.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn power_off() {
    // SAFETY: reboot takes three arguments. On a kernel that accepts it the
    // call never returns; on a kernel that refuses it, the result is an error
    // number and the caller continues.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") SYS_REBOOT as i64 => _,
            in("rdi") LINUX_REBOOT_MAGIC1,
            in("rsi") LINUX_REBOOT_MAGIC2,
            in("rdx") LINUX_REBOOT_CMD_POWER_OFF,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
}

/// Ask the machine to power off, on a build that cannot make the call.
///
/// The host build of the library has no guest to power off, so the call is
/// not there. A program that only runs in the guest never sees it.
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub fn power_off() {}
