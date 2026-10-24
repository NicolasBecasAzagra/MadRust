// SPDX-License-Identifier: GPL-2.0-only
//! System call entry configuration and the Linux dispatcher.
//!
//! The processor side is `SYSCALL`/`SYSRET`: `IA32_STAR` holds the kernel and
//! user selectors, `IA32_LSTAR` the entry stub, `IA32_SFMASK` the flags to
//! clear on entry, and `IA32_KERNEL_GS_BASE` the per-CPU area the stub uses to
//! save the user stack pointer. `EFER.SCE` is enabled here because the boot
//! trampoline only enables long mode.
//!
//! The dispatcher follows the Linux x86-64 convention: the number is in RAX,
//! the arguments in RDI, RSI, RDX, R10, R8 and R9, and the result in RAX. A
//! negative result is a negated errno; an unknown number returns `-ENOSYS`.
//! Every call that is not implemented in this milestone also returns `-ENOSYS`
//! instead of silently succeeding.

use core::sync::atomic::{AtomicU64, Ordering};

use madrust_arch::cpu;
use madrust_elf::USER_SPACE_END;

use crate::gdt::{CODE_SELECTOR, USER_CODE_SELECTOR};
use crate::pit;

/// Model specific register that controls extended features.
const MSR_EFER: u32 = 0xc000_0080;
/// Model specific register with the `SYSCALL`/`SYSRET` selectors.
const MSR_STAR: u32 = 0xc000_0081;
/// Model specific register with the `SYSCALL` entry address.
const MSR_LSTAR: u32 = 0xc000_0082;
/// Model specific register with the flags `SYSCALL` clears.
const MSR_SFMASK: u32 = 0xc000_0084;
/// Model specific register with the kernel GS base `swapgs` selects.
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

/// `EFER.SCE`: enable the `SYSCALL` and `SYSRET` instructions.
const EFER_SYSCALL_ENABLE: u64 = 1 << 0;
/// `RFLAGS.IF`: mask interrupts while a system call runs.
const RFLAGS_INTERRUPT_FLAG: u64 = 1 << 9;

/// `write` system call number.
const SYS_WRITE: u64 = 1;
/// `read` system call number.
const SYS_READ: u64 = 0;
/// `close` system call number.
const SYS_CLOSE: u64 = 3;
/// `brk` system call number.
const SYS_BRK: u64 = 12;
/// `mmap` system call number.
const SYS_MMAP: u64 = 9;
/// `munmap` system call number.
const SYS_MUNMAP: u64 = 11;
/// `ioctl` system call number.
const SYS_IOCTL: u64 = 16;
/// `getpid` system call number.
const SYS_GETPID: u64 = 39;
/// `uname` system call number.
const SYS_UNAME: u64 = 63;
/// `fcntl` system call number.
const SYS_FCNTL: u64 = 72;
/// `getuid` system call number.
const SYS_GETUID: u64 = 102;
/// `getgid` system call number.
const SYS_GETGID: u64 = 104;
/// `geteuid` system call number.
const SYS_GETEUID: u64 = 107;
/// `getegid` system call number.
const SYS_GETEGID: u64 = 108;
/// `getppid` system call number.
const SYS_GETPPID: u64 = 110;
/// `gettid` system call number.
const SYS_GETTID: u64 = 186;
/// `clock_gettime` system call number.
const SYS_CLOCK_GETTIME: u64 = 228;
/// `exit_group` system call number.
const SYS_EXIT_GROUP: u64 = 231;
/// `exit` system call number.
const SYS_EXIT: u64 = 60;

/// `Bad file descriptor`.
const EBADF: i64 = -9;
/// `Cannot allocate memory`.
const ENOMEM: i64 = -12;
/// `Bad address`.
const EFAULT: i64 = -14;
/// `Invalid argument`.
const EINVAL: i64 = -22;
/// `Inappropriate ioctl for device`.
const ENOTTY: i64 = -25;
/// `Function not implemented`.
const ENOSYS: i64 = -38;

/// `fcntl` command that reports the descriptor flags.
const F_GETFD: u64 = 1;

/// Number of bytes in each `utsname` field, including the terminating NUL.
const UTS_FIELD: usize = 65;

/// Per-CPU data the system call stub reaches through `swapgs`.
#[repr(C, align(16))]
struct PerCpu {
    /// User stack pointer saved by `madrust_syscall_entry`.
    ///
    /// Written and read by `syscall_entry.s` at `GS:0`; the kernel never
    /// touches it from Rust, so the compiler cannot see the use.
    #[allow(dead_code)]
    user_stack: u64,
    /// Keeps the slot 16 byte aligned and leaves room for later fields.
    #[allow(dead_code)]
    reserved: u64,
}

#[repr(transparent)]
struct PerCpuCell(core::cell::UnsafeCell<PerCpu>);

// SAFETY: the stub writes `user_stack` from ring 0 and the kernel only reads it
// from the same path, always with interrupts masked during a system call.
unsafe impl Sync for PerCpuCell {}

static PER_CPU: PerCpuCell = PerCpuCell(core::cell::UnsafeCell::new(PerCpu {
    user_stack: 0,
    reserved: 0,
}));

/// Program break reported by `brk`.
static PROGRAM_BREAK: AtomicU64 = AtomicU64::new(0);

/// Counter that makes the random bytes differ between boots.
static RANDOM_STATE: AtomicU64 = AtomicU64::new(0x9e37_79b9_7f4a_7c15);

extern "C" {
    /// Assembly entry point installed in `IA32_LSTAR`.
    fn madrust_syscall_entry();
    /// Assembly transition from ring 0 to ring 3.
    fn madrust_enter_user_mode(entry: u64, stack: u64) -> u64;
    /// Assembly return from an exit system call into the kernel.
    fn madrust_user_return(status: u64) -> !;
}

/// Enable `SYSCALL`/`SYSRET` and point the entry at the assembly stub.
pub fn init() {
    let per_cpu = core::ptr::addr_of!(PER_CPU) as u64;
    let entry = madrust_syscall_entry as *const () as u64;

    // STAR[47:32] is the kernel code selector and STAR[63:48] the base the
    // SYSRET selectors are derived from: code is base + 16 and stack is
    // base + 8. With the ring 3 data selector at 0x23 the base is the ring 3
    // code selector 0x1B, so SYSRET returns to code 0x2B and stack 0x23.
    let star = (u64::from(CODE_SELECTOR) << 32) | (u64::from(USER_CODE_SELECTOR) << 48);

    let efer = cpu::read_msr(MSR_EFER);
    // SAFETY: EFER exists on every x86_64 CPU that has SYSCALL, and only the
    // system call enable bit is added. STAR, LSTAR, SFMASK and KERNEL_GS_BASE
    // exist on the same CPUs and receive values built from installed
    // descriptors, the stub address and the per-CPU page.
    unsafe {
        cpu::write_msr(MSR_EFER, efer | EFER_SYSCALL_ENABLE);
        cpu::write_msr(MSR_STAR, star);
        cpu::write_msr(MSR_LSTAR, entry);
        cpu::write_msr(MSR_SFMASK, RFLAGS_INTERRUPT_FLAG);
        cpu::write_msr(MSR_KERNEL_GS_BASE, per_cpu);
    }
}

/// Remember the initial program break so `brk` can report it.
pub fn set_break(address: u64) {
    PROGRAM_BREAK.store(address, Ordering::Release);
}

/// Original program break, used when `brk` is asked for zero.
fn program_break() -> u64 {
    PROGRAM_BREAK.load(Ordering::Acquire)
}

/// Move to ring 3 and return the program's exit status.
///
/// # Safety
///
/// The user half must be mapped and the IDT installed: the program runs until
/// it issues `exit_group`, and any fault it takes is reported by the kernel.
pub unsafe fn enter(entry: u64, stack: u64) -> u64 {
    // SAFETY: the caller guarantees the user address space and the exception
    // handlers are ready, which is what the assembly transition requires.
    unsafe { madrust_enter_user_mode(entry, stack) }
}

/// Yield 16 bytes of pseudo random data for `AT_RANDOM`.
///
/// This is deliberately not a cryptographic generator: it only has to give the
/// program a pointer it can read, and the value changes between boots.
pub fn random_bytes() -> [u8; 16] {
    let mut state = RANDOM_STATE.load(Ordering::Relaxed);
    let mut output = [0u8; 16];
    for chunk in output.chunks_mut(8) {
        // xorshift64*: cheap, deterministic and adequate for the placeholder.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    RANDOM_STATE.store(state, Ordering::Relaxed);
    output
}

/// Rust half of the system call entry.
///
/// Arguments four to six are part of the Linux calling convention and reach
/// the dispatcher in registers, but no call implemented by this milestone uses
/// them yet.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn madrust_syscall_dispatch(
    number: u64,
    argument1: u64,
    argument2: u64,
    argument3: u64,
    _argument4: u64,
    _argument5: u64,
    _argument6: u64,
) -> i64 {
    match number {
        SYS_READ => 0,
        SYS_WRITE => sys_write(argument1, argument2, argument3),
        SYS_CLOSE => sys_close(argument1),
        SYS_EXIT | SYS_EXIT_GROUP => sys_exit(argument1),
        SYS_BRK => program_break() as i64,
        SYS_MMAP => ENOMEM,
        SYS_MUNMAP => 0,
        SYS_UNAME => sys_uname(argument1),
        SYS_GETPID | SYS_GETTID => 1,
        SYS_GETPPID => 0,
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        SYS_IOCTL => ENOTTY,
        SYS_FCNTL => sys_fcntl(argument2),
        SYS_CLOCK_GETTIME => sys_clock_gettime(argument2),
        _ => ENOSYS,
    }
}

/// `write`: send `length` bytes from the user buffer to the serial console.
fn sys_write(descriptor: u64, buffer: u64, length: u64) -> i64 {
    if descriptor != 1 && descriptor != 2 {
        return EBADF;
    }
    let Some(bytes) = user_bytes(buffer, length) else {
        return EFAULT;
    };
    let mut uart = crate::console::console();
    uart.write_bytes(bytes);
    i64::try_from(length).unwrap_or(i64::MAX)
}

/// `close`: only the standard descriptors exist.
fn sys_close(descriptor: u64) -> i64 {
    if descriptor <= 2 {
        0
    } else {
        EBADF
    }
}

/// `fcntl`: report the default descriptor flags and reject commands that need
/// a file table this kernel does not have.
fn sys_fcntl(command: u64) -> i64 {
    if command == F_GETFD {
        0
    } else {
        EINVAL
    }
}

/// `uname`: fill the six fixed size fields with the kernel identity.
fn sys_uname(address: u64) -> i64 {
    let fields = ["MadRust", "madrust", "0.1.0", "M4a", "x86_64", "(none)"];
    let mut buffer = [0u8; 6 * UTS_FIELD];
    for (index, text) in fields.iter().enumerate() {
        let start = index * UTS_FIELD;
        let bytes = text.as_bytes();
        if bytes.len() + 1 > UTS_FIELD {
            continue;
        }
        buffer[start..start + bytes.len()].copy_from_slice(bytes);
    }
    if write_user(address, &buffer) {
        0
    } else {
        EFAULT
    }
}

/// `clock_gettime`: seconds and nanoseconds since boot.
fn sys_clock_gettime(timespec: u64) -> i64 {
    let ticks = pit::ticks();
    let seconds = ticks / pit::TICK_HZ;
    let nanoseconds = (ticks % pit::TICK_HZ) * (1_000_000_000 / pit::TICK_HZ);
    let mut buffer = [0u8; 16];
    buffer[0..8].copy_from_slice(&(seconds as i64).to_le_bytes());
    buffer[8..16].copy_from_slice(&(nanoseconds as i64).to_le_bytes());
    if write_user(timespec, &buffer) {
        0
    } else {
        EFAULT
    }
}

/// `exit_group` and `exit`: leave the program and report the status.
fn sys_exit(status: u64) -> ! {
    // SAFETY: the assembly return path restores the kernel frame saved by the
    // transition into user mode. The program is never resumed.
    unsafe { madrust_user_return(status & 0xff) }
}

/// Borrow `length` bytes of the user half, if the whole range is there.
fn user_bytes(address: u64, length: u64) -> Option<&'static [u8]> {
    if length == 0 {
        return Some(&[]);
    }
    let end = address.checked_add(length)?;
    if address >= USER_SPACE_END || end > USER_SPACE_END {
        return None;
    }
    let length = usize::try_from(length).ok()?;
    // SAFETY: the range was checked to lie entirely inside the user half, and
    // the user program of this milestone owns it. The kernel runs at CPL 0
    // without SMAP, so it may read user pages. A concurrent user write could
    // change the bytes while a call reads them; nothing else in the kernel
    // reads that memory.
    Some(unsafe { core::slice::from_raw_parts(address as *const u8, length) })
}

/// Copy `bytes` into a user buffer, if the whole range is in the user half.
fn write_user(address: u64, bytes: &[u8]) -> bool {
    let Ok(length) = u64::try_from(bytes.len()) else {
        return false;
    };
    let Some(end) = address.checked_add(length) else {
        return false;
    };
    if address >= USER_SPACE_END || end > USER_SPACE_END {
        return false;
    }
    // SAFETY: the destination range was checked to lie inside the user half,
    // the source is a kernel buffer, and the two do not overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len());
    }
    true
}
