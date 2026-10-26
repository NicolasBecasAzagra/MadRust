// SPDX-License-Identifier: GPL-2.0-only
//! The inference runtime binary.
//!
//! The prompt is already encoded because the runtime has no pre-tokenizer. A
//! model config can carry `prompt_ids`; otherwise the runtime uses the baked
//! reference prompt, so the same binary serves the small synthetic model and
//! the real checkpoint.
//!
//! An optional positional argument is the number of tokens to generate, eight
//! when it is absent. The guest passes no arguments, so its run is unchanged;
//! a native run can ask for more tokens to make its token rate stable. The
//! model root comes from `MADRUST_INFER_ROOT` when the environment defines it,
//! and from the initramfs root otherwise.

#![deny(unsafe_op_in_unsafe_fn)]

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

/// Ask the machine to go down, so the kernel does not panic when the last
/// process leaves.
///
/// Mainline Linux panics with "Attempted to kill init!" when PID 1 exits, and
/// the runtime is PID 1 there. Asking the kernel to power the machine off ends
/// the run without that panic. A kernel that does not know the call answers
/// with an error and the process leaves the ordinary way, which is what MadRust
/// does. The benchmark workload uses the same approach.
#[cfg(target_os = "linux")]
fn power_off() {
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: reboot takes three arguments. On a kernel that accepts it the
        // call never returns; on a kernel that refuses it, the result is an
        // error number and the caller continues.
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") SYS_REBOOT as i64 => _,
                in("rdi") LINUX_REBOOT_MAGIC1,
                in("rsi") LINUX_REBOOT_CMD_POWER_OFF,
                in("rdx") LINUX_REBOOT_MAGIC2,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
    }
}

/// Flush the log, ask the machine to go down, and leave with `status`.
///
/// The flush happens first: the last line has to reach the host before the
/// reboot call stops the machine.
#[cfg(target_os = "linux")]
fn leave(status: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    power_off();
    std::process::exit(status)
}

#[cfg(target_os = "linux")]
fn main() {
    let count = match madrust_infer::cli::token_count(std::env::args_os().skip(1)) {
        Ok(count) => count,
        Err(error) => {
            eprintln!("INFER ERR {error}");
            leave(2);
        }
    };
    if let Err(error) = madrust_infer::runtime::run(count) {
        eprintln!("INFER ERR {error}");
        leave(1);
    }
    leave(0);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("madrust-infer: build the runtime for x86_64-unknown-linux-musl");
    std::process::exit(2);
}
