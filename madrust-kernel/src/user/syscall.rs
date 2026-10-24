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
//! negative result is a negated errno; a call this kernel does not implement
//! returns `-ENOSYS` and prints its number, so a standard library program that
//! needs one more call makes the gap visible in the boot log.
//!
//! ## What the table covers
//!
//! The calls fall into four groups:
//!
//! * Process identity and lifetime: `getpid`, `gettid`, the ids, `exit`,
//!   `exit_group`, `set_tid_address`, `set_robust_list`, `kill`, `tgkill`.
//! * Memory: `brk`, `mmap`, `munmap`, `mprotect`, `madvise`, `mremap`, with
//!   the implementation in [`super::vm`].
//! * Threads and signals: `arch_prctl` (thread local storage), the signal
//!   calls, `futex` (with the wait queue in [`super::futex`]) and `rseq`.
//! * Input and output: the descriptor calls, `write`, `read`, `uname`, the
//!   clocks, `getrandom`, `nanosleep` and `getrusage`.
//!
//! There is no filesystem, so `openat`, `newfstatat`, `statx`, `getdents64`
//! and `readlink` report `ENOENT` or `ENOTDIR`: the calls are implemented,
//! their answer is just "nothing is mounted".

use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

use madrust_arch::cpu;
use madrust_elf::USER_SPACE_END;
use madrust_mm::paging::PAGE_SIZE;

use crate::gdt::{CODE_SELECTOR, USER_CODE_SELECTOR};
use crate::pit;

use super::errno::*;
use super::{fd, futex, paging, vm};

/// Model specific register that controls extended features.
const MSR_EFER: u32 = 0xc000_0080;
/// Model specific register with the `SYSCALL`/`SYSRET` selectors.
const MSR_STAR: u32 = 0xc000_0081;
/// Model specific register with the `SYSCALL` entry address.
const MSR_LSTAR: u32 = 0xc000_0082;
/// Model specific register with the flags `SYSCALL` clears.
const MSR_SFMASK: u32 = 0xc000_0084;
/// Model specific register with the user thread local storage base.
const MSR_FS_BASE: u32 = 0xc000_0100;
/// Model specific register with the kernel GS base `swapgs` selects.
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

/// `EFER.SCE`: enable the `SYSCALL` and `SYSRET` instructions.
const EFER_SYSCALL_ENABLE: u64 = 1 << 0;
/// `RFLAGS.IF`: mask interrupts while a system call runs.
const RFLAGS_INTERRUPT_FLAG: u64 = 1 << 9;

/// `arch_prctl` operation: set the thread local storage base.
const ARCH_SET_FS: u64 = 0x1002;
/// `arch_prctl` operation: read the thread local storage base.
const ARCH_GET_FS: u64 = 0x1003;

/// `FUTEX_WAIT`: sleep while the futex word keeps the expected value.
const FUTEX_WAIT: u64 = 0;
/// `FUTEX_WAKE`: wake waiters of a futex word.
const FUTEX_WAKE: u64 = 1;
/// `FUTEX_WAIT_BITSET`: the form the Rust standard library uses.
const FUTEX_WAIT_BITSET: u64 = 9;
/// `FUTEX_WAKE_BITSET`: the matching wake operation.
const FUTEX_WAKE_BITSET: u64 = 10;
/// Mask that selects the operation and hides the private and clock flags.
const FUTEX_OPERATION_MASK: u64 = 0x7f;

/// `read` system call number.
const SYS_READ: u64 = 0;
/// `write` system call number.
const SYS_WRITE: u64 = 1;
/// `close` system call number.
const SYS_CLOSE: u64 = 3;
/// `fstat` system call number.
const SYS_FSTAT: u64 = 5;
/// `poll` system call number.
const SYS_POLL: u64 = 7;
/// `mmap` system call number.
const SYS_MMAP: u64 = 9;
/// `mprotect` system call number.
const SYS_MPROTECT: u64 = 10;
/// `munmap` system call number.
const SYS_MUNMAP: u64 = 11;
/// `brk` system call number.
const SYS_BRK: u64 = 12;
/// `rt_sigaction` system call number.
const SYS_RT_SIGACTION: u64 = 13;
/// `rt_sigprocmask` system call number.
const SYS_RT_SIGPROCMASK: u64 = 14;
/// `ioctl` system call number.
const SYS_IOCTL: u64 = 16;
/// `sched_yield` system call number.
const SYS_SCHED_YIELD: u64 = 24;
/// `mremap` system call number.
const SYS_MREMAP: u64 = 25;
/// `madvise` system call number.
const SYS_MADVISE: u64 = 28;
/// `dup` system call number.
const SYS_DUP: u64 = 32;
/// `dup2` system call number.
const SYS_DUP2: u64 = 33;
/// `nanosleep` system call number.
const SYS_NANOSLEEP: u64 = 35;
/// `getpid` system call number.
const SYS_GETPID: u64 = 39;
/// `exit` system call number.
const SYS_EXIT: u64 = 60;
/// `kill` system call number.
const SYS_KILL: u64 = 62;
/// `uname` system call number.
const SYS_UNAME: u64 = 63;
/// `fcntl` system call number.
const SYS_FCNTL: u64 = 72;
/// `getcwd` system call number.
const SYS_GETCWD: u64 = 79;
/// `readlink` system call number.
const SYS_READLINK: u64 = 89;
/// `gettimeofday` system call number.
const SYS_GETTIMEOFDAY: u64 = 96;
/// `getrusage` system call number.
const SYS_GETRUSAGE: u64 = 98;
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
/// `sigaltstack` system call number.
const SYS_SIGALTSTACK: u64 = 131;
/// `prctl` system call number.
const SYS_PRCTL: u64 = 157;
/// `arch_prctl` system call number.
const SYS_ARCH_PRCTL: u64 = 158;
/// `gettid` system call number.
const SYS_GETTID: u64 = 186;
/// `futex` system call number.
const SYS_FUTEX: u64 = 202;
/// `sched_getaffinity` system call number.
const SYS_SCHED_GETAFFINITY: u64 = 204;
/// `getdents64` system call number.
const SYS_GETDENTS64: u64 = 217;
/// `set_tid_address` system call number.
const SYS_SET_TID_ADDRESS: u64 = 218;
/// `clock_gettime` system call number.
const SYS_CLOCK_GETTIME: u64 = 228;
/// `clock_nanosleep` system call number.
const SYS_CLOCK_NANOSLEEP: u64 = 230;
/// `exit_group` system call number.
const SYS_EXIT_GROUP: u64 = 231;
/// `tgkill` system call number.
const SYS_TGKILL: u64 = 234;
/// `openat` system call number.
const SYS_OPENAT: u64 = 257;
/// `newfstatat` system call number.
const SYS_NEWFSTATAT: u64 = 262;
/// `set_robust_list` system call number.
const SYS_SET_ROBUST_LIST: u64 = 273;
/// `dup3` system call number.
const SYS_DUP3: u64 = 292;
/// `pipe2` system call number.
const SYS_PIPE2: u64 = 293;
/// `prlimit64` system call number.
const SYS_PRLIMIT64: u64 = 302;
/// `getrandom` system call number.
const SYS_GETRANDOM: u64 = 318;
/// `membarrier` system call number.
const SYS_MEMBARRIER: u64 = 324;
/// `statx` system call number.
const SYS_STATX: u64 = 332;
/// `rseq` system call number.
const SYS_RSEQ: u64 = 334;

/// `RLIMIT_STACK` resource number.
const RLIMIT_STACK: u64 = 3;
/// `RLIMIT_NOFILE` resource number.
const RLIMIT_NOFILE: u64 = 7;

/// Stack limit reported to the program, 8 MiB.
const STACK_LIMIT: u64 = 8 * 1024 * 1024;
/// Open file limit reported to the program.
const NOFILE_LIMIT: u64 = 1024;

/// Number of bytes in each `utsname` field, including the terminating NUL.
const UTS_FIELD: usize = 65;

/// Size of `struct timespec` and `struct timeval`.
const TIME_SIZE: usize = 16;

/// Number of bytes `getrandom` writes per pseudo random draw.
const RANDOM_CHUNK: usize = 16;

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
    vm::set_break(address);
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
    let mut output = [0u8; RANDOM_CHUNK];
    random_fill(&mut output);
    output
}

/// Fill `bytes` from the pseudo random generator.
fn random_fill(bytes: &mut [u8]) {
    let mut state = RANDOM_STATE.load(Ordering::Relaxed);
    for chunk in bytes.chunks_mut(8) {
        // xorshift64*: cheap, deterministic and adequate for the placeholder.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
    }
    RANDOM_STATE.store(state, Ordering::Relaxed);
}

/// Rust half of the system call entry.
///
/// The arguments arrive in the Linux order: `argument4` was R10, `argument5`
/// was R8 and `argument6` was R9.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn madrust_syscall_dispatch(
    number: u64,
    argument1: u64,
    argument2: u64,
    argument3: u64,
    argument4: u64,
    argument5: u64,
    argument6: u64,
) -> i64 {
    match number {
        SYS_READ => sys_read(argument1, argument2, argument3),
        SYS_WRITE => sys_write(argument1, argument2, argument3),
        SYS_OPENAT => ENOENT,
        SYS_CLOSE => fd::close(argument1),
        SYS_FSTAT => sys_fstat(argument1, argument2),
        SYS_NEWFSTATAT | SYS_STATX => ENOENT,
        SYS_GETDENTS64 => ENOTDIR,
        SYS_POLL => sys_poll(argument1, argument2),
        SYS_IOCTL => sys_ioctl(argument1),
        SYS_FCNTL => fd::fcntl(argument1, argument2, argument3),
        SYS_PRLIMIT64 => sys_prlimit64(argument2, argument3, argument4),
        SYS_SCHED_GETAFFINITY => sys_sched_getaffinity(argument2, argument3),
        SYS_SCHED_YIELD => 0,
        SYS_BRK => i64::try_from(vm::sys_brk(argument1)).unwrap_or(i64::MAX),
        SYS_MMAP => sys_mmap(
            argument1, argument2, argument3, argument4, argument5, argument6,
        ),
        SYS_MUNMAP => result(vm::munmap(argument1, argument2)),
        SYS_MPROTECT => result(vm::mprotect(argument1, argument2, argument3)),
        SYS_MADVISE => result(vm::madvise(argument1, argument2)),
        SYS_MREMAP => sys_mremap(argument1, argument2, argument3, argument4, argument5),
        SYS_ARCH_PRCTL => sys_arch_prctl(argument1, argument2),
        SYS_RT_SIGACTION => sys_rt_sigaction(argument1, argument3),
        SYS_RT_SIGPROCMASK => sys_rt_sigprocmask(argument3),
        SYS_SIGALTSTACK => sys_sigaltstack(argument2),
        SYS_SET_TID_ADDRESS => 1,
        SYS_SET_ROBUST_LIST => 0,
        SYS_RSEQ => 0,
        SYS_PRCTL => 0,
        SYS_UNAME => sys_uname(argument1),
        SYS_CLOCK_GETTIME => sys_clock_gettime(argument2),
        SYS_GETTIMEOFDAY => sys_gettimeofday(argument1, argument2),
        SYS_GETRANDOM => sys_getrandom(argument1, argument2),
        SYS_FUTEX => sys_futex(argument1, argument2, argument3, argument4),
        SYS_EXIT | SYS_EXIT_GROUP => sys_exit(argument1),
        SYS_GETPID | SYS_GETTID => 1,
        SYS_GETPPID => 0,
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        SYS_READLINK => EINVAL,
        SYS_GETCWD => sys_getcwd(argument1, argument2),
        SYS_PIPE2 => sys_pipe2(argument1),
        SYS_DUP => fd::dup(argument1, argument2),
        SYS_DUP2 => fd::dup_to(argument1, argument2),
        SYS_DUP3 => {
            if argument3 == 0 {
                fd::dup_to(argument1, argument2)
            } else {
                EINVAL
            }
        }
        SYS_KILL => sys_kill(argument1),
        SYS_TGKILL => sys_tgkill(argument1, argument2),
        SYS_NANOSLEEP => sys_nanosleep(argument1, argument2),
        SYS_CLOCK_NANOSLEEP => sys_nanosleep(argument3, argument4),
        SYS_GETRUSAGE => sys_getrusage(argument2),
        SYS_MEMBARRIER => 0,
        _ => {
            crate::kprintln!("syscall unimplemented: {}", number);
            ENOSYS
        }
    }
}

/// Turn a `Result` that carries no value into the dispatcher return type.
fn result(operation: Result<(), i64>) -> i64 {
    match operation {
        Ok(()) => 0,
        Err(error) => error,
    }
}

/// `write`: send `length` bytes from the user buffer to a descriptor.
fn sys_write(descriptor: u64, buffer: u64, length: u64) -> i64 {
    let Some(bytes) = user_bytes(buffer, length) else {
        return EFAULT;
    };
    fd::write(descriptor, bytes)
}

/// `read`: take bytes from a descriptor into the user buffer.
fn sys_read(descriptor: u64, buffer: u64, length: u64) -> i64 {
    let Some(length) = user_range(buffer, length) else {
        return EFAULT;
    };
    if length == 0 {
        return 0;
    }
    let mut kernel_buffer = [0u8; 256];
    let count = fd::read(descriptor, &mut kernel_buffer[..length.min(256)]);
    if count <= 0 {
        return count;
    }
    let Ok(count) = usize::try_from(count) else {
        return EFAULT;
    };
    if !write_user_bytes(buffer, &kernel_buffer[..count]) {
        return EFAULT;
    }
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// `poll`: report which descriptors are ready.
///
/// The standard library checks descriptors zero to two with a zero timeout
/// during startup, so the call has to answer instead of failing. A console
/// descriptor is always ready for its direction; a pipe reports ready when it
/// has data or room. The timeout is ignored because no wait is ever needed:
/// every descriptor this kernel can report is ready or permanently empty.
fn sys_poll(descriptors: u64, count: u64) -> i64 {
    /// `POLLIN`: data may be read.
    const POLLIN: i16 = 0x001;
    /// `POLLOUT`: data may be written.
    const POLLOUT: i16 = 0x004;
    /// `POLLNVAL`: the descriptor is not open.
    const POLLNVAL: i16 = 0x020;

    let Ok(count) = usize::try_from(count) else {
        return EINVAL;
    };
    if count == 0 {
        return 0;
    }
    if count > 128 {
        return EINVAL;
    }
    if user_range(descriptors, (count * 8) as u64).is_none() {
        return EFAULT;
    }

    let mut ready = 0;
    for index in 0..count {
        let entry = descriptors + (index * 8) as u64;
        let Some(descriptor) = read_user::<i32>(entry) else {
            return EFAULT;
        };
        let Some(events) = read_user::<i16>(entry + 4) else {
            return EFAULT;
        };
        let revents = if descriptor < 0 {
            0
        } else if !fd::is_open(descriptor as u64) {
            POLLNVAL
        } else {
            let mut ready = 0;
            if fd::readable(descriptor as u64) {
                ready |= events & POLLIN;
            }
            if fd::writable(descriptor as u64) {
                ready |= events & POLLOUT;
            }
            ready
        };
        if !write_user_bytes(entry + 6, &revents.to_le_bytes()) {
            return EFAULT;
        }
        if revents != 0 {
            ready += 1;
        }
    }
    ready
}

/// `fstat`: describe one descriptor.
fn sys_fstat(descriptor: u64, address: u64) -> i64 {
    match fd::stat(descriptor) {
        Ok(block) => {
            if write_user_bytes(address, &block) {
                0
            } else {
                EFAULT
            }
        }
        Err(error) => error,
    }
}

/// `ioctl`: no control operation is supported, but the descriptor must exist.
fn sys_ioctl(descriptor: u64) -> i64 {
    if fd::is_open(descriptor) {
        fd::ioctl_error()
    } else {
        EBADF
    }
}

/// `prlimit64`: report the fixed limits, reject a change.
fn sys_prlimit64(resource: u64, new_limit: u64, old_limit: u64) -> i64 {
    if new_limit != 0 {
        return fd::limit_change_error();
    }
    if old_limit == 0 {
        return 0;
    }
    let current = match resource {
        RLIMIT_STACK => STACK_LIMIT,
        RLIMIT_NOFILE => NOFILE_LIMIT,
        _ => u64::MAX,
    };
    let mut block = [0u8; 16];
    block[0..8].copy_from_slice(&current.to_le_bytes());
    block[8..16].copy_from_slice(&current.to_le_bytes());
    if write_user_bytes(old_limit, &block) {
        0
    } else {
        EFAULT
    }
}

/// `sched_getaffinity`: report a single online CPU.
fn sys_sched_getaffinity(size: u64, address: u64) -> i64 {
    if size < 8 {
        return EINVAL;
    }
    if !write_user_bytes(address, &1u64.to_le_bytes()) {
        return EFAULT;
    }
    8
}

/// `mmap`: create a mapping through [`vm::mmap`].
fn sys_mmap(
    address: u64,
    length: u64,
    protection: u64,
    flags: u64,
    descriptor: u64,
    offset: u64,
) -> i64 {
    let descriptor = i64::try_from(descriptor).unwrap_or(-1);
    match vm::mmap(address, length, protection, flags, descriptor, offset) {
        Ok(base) => i64::try_from(base).unwrap_or(i64::MAX),
        Err(error) => error,
    }
}

/// `mremap`: resize a mapping through [`vm::mremap`].
fn sys_mremap(address: u64, old_length: u64, new_length: u64, flags: u64, new_address: u64) -> i64 {
    match vm::mremap(address, old_length, new_length, flags, new_address) {
        Ok(base) => i64::try_from(base).unwrap_or(i64::MAX),
        Err(error) => error,
    }
}

/// `arch_prctl`: set or read the thread local storage base.
///
/// `ARCH_SET_GS` is rejected: this kernel keeps its per-CPU area in the GS base
/// that `swapgs` exchanges, so a user value there would replace the kernel
/// pointer on the next system call.
fn sys_arch_prctl(operation: u64, address: u64) -> i64 {
    match operation {
        ARCH_SET_FS => {
            if address >= USER_SPACE_END {
                return EPERM;
            }
            // SAFETY: FS_BASE exists on every x86_64 CPU. The value is a user
            // address, so the kernel keeps its own segment bases unchanged.
            unsafe { cpu::write_msr(MSR_FS_BASE, address) };
            0
        }
        ARCH_GET_FS => {
            let base = cpu::read_msr(MSR_FS_BASE);
            if write_user_bytes(address, &base.to_le_bytes()) {
                0
            } else {
                EFAULT
            }
        }
        _ => EINVAL,
    }
}

/// `rt_sigaction`: accept the handler, because no signal is ever delivered.
fn sys_rt_sigaction(_signal: u64, old_action: u64) -> i64 {
    if old_action == 0 {
        return 0;
    }
    // `struct sigaction` on x86-64 is 32 bytes: handler, flags, restorer and
    // the mask. Reporting a zeroed action means "default".
    if write_user_bytes(old_action, &[0u8; 32]) {
        0
    } else {
        EFAULT
    }
}

/// `rt_sigprocmask`: report an empty mask, accept any new one.
fn sys_rt_sigprocmask(old_mask: u64) -> i64 {
    if old_mask == 0 {
        return 0;
    }
    if write_user_bytes(old_mask, &0u64.to_le_bytes()) {
        0
    } else {
        EFAULT
    }
}

/// `sigaltstack`: report a disabled alternate stack, accept any new one.
fn sys_sigaltstack(old_stack: u64) -> i64 {
    if old_stack == 0 {
        return 0;
    }
    // `stack_t`: ss_sp, ss_flags, ss_size. `SS_DISABLE` is 2.
    let mut block = [0u8; 24];
    block[8..12].copy_from_slice(&2u32.to_le_bytes());
    if write_user_bytes(old_stack, &block) {
        0
    } else {
        EFAULT
    }
}

/// `uname`: fill the six fixed size fields with the kernel identity.
fn sys_uname(address: u64) -> i64 {
    let fields = [
        "Linux",
        "madrust",
        "6.1.0-madrust",
        "M4b",
        "x86_64",
        "(none)",
    ];
    let mut buffer = [0u8; 6 * UTS_FIELD];
    for (index, text) in fields.iter().enumerate() {
        let start = index * UTS_FIELD;
        let bytes = text.as_bytes();
        if bytes.len() + 1 > UTS_FIELD {
            continue;
        }
        buffer[start..start + bytes.len()].copy_from_slice(bytes);
    }
    if write_user_bytes(address, &buffer) {
        0
    } else {
        EFAULT
    }
}

/// `clock_gettime`: seconds and nanoseconds since boot.
fn sys_clock_gettime(timespec: u64) -> i64 {
    let (seconds, nanoseconds) = clock_now();
    let mut buffer = [0u8; TIME_SIZE];
    buffer[0..8].copy_from_slice(&(seconds as i64).to_le_bytes());
    buffer[8..16].copy_from_slice(&(nanoseconds as i64).to_le_bytes());
    if write_user_bytes(timespec, &buffer) {
        0
    } else {
        EFAULT
    }
}

/// `gettimeofday`: the same clock in `struct timeval` form.
fn sys_gettimeofday(timeval: u64, timezone: u64) -> i64 {
    let (seconds, nanoseconds) = clock_now();
    let mut buffer = [0u8; TIME_SIZE];
    buffer[0..8].copy_from_slice(&(seconds as i64).to_le_bytes());
    buffer[8..16].copy_from_slice(&((nanoseconds / 1000) as i64).to_le_bytes());
    if !write_user_bytes(timeval, &buffer) {
        return EFAULT;
    }
    if timezone != 0 && !write_user_bytes(timezone, &[0u8; 8]) {
        return EFAULT;
    }
    0
}

/// Seconds and nanoseconds since boot from the timer ticks.
fn clock_now() -> (u64, u64) {
    let ticks = pit::ticks();
    let seconds = ticks / pit::TICK_HZ;
    let nanoseconds = (ticks % pit::TICK_HZ) * (1_000_000_000 / pit::TICK_HZ);
    (seconds, nanoseconds)
}

/// `getrandom`: fill the user buffer from the pseudo random generator.
fn sys_getrandom(buffer: u64, length: u64) -> i64 {
    let Some(length) = user_range(buffer, length) else {
        return EFAULT;
    };
    let mut offset = 0;
    while offset < length {
        let mut chunk = [0u8; RANDOM_CHUNK];
        random_fill(&mut chunk);
        let count = (length - offset).min(RANDOM_CHUNK);
        if !write_user_bytes(buffer + offset as u64, &chunk[..count]) {
            return EFAULT;
        }
        offset += count;
    }
    i64::try_from(length).unwrap_or(i64::MAX)
}

/// `futex`: wait on or wake a futex word.
fn sys_futex(address: u64, operation: u64, expected: u64, timeout: u64) -> i64 {
    let Some(value) = read_user_u32(address) else {
        return EFAULT;
    };
    match operation & FUTEX_OPERATION_MASK {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let timeout = match user_timeout(timeout) {
                Ok(timeout) => timeout,
                Err(error) => return error,
            };
            let Ok(expected) = u32::try_from(expected) else {
                return EINVAL;
            };
            futex::wait(address, value, expected, timeout)
        }
        FUTEX_WAKE | FUTEX_WAKE_BITSET => futex::wake(address, expected),
        _ => {
            crate::kprintln!("syscall unimplemented: {} (futex {})", SYS_FUTEX, operation);
            ENOSYS
        }
    }
}

/// Read an optional `struct timespec` and convert it to nanoseconds.
///
/// A null pointer means "wait without a timeout".
fn user_timeout(address: u64) -> Result<Option<u64>, i64> {
    if address == 0 {
        return Ok(None);
    }
    let Some(seconds) = read_user::<i64>(address) else {
        return Err(EFAULT);
    };
    let Some(nanoseconds) = read_user::<i64>(address + 8) else {
        return Err(EFAULT);
    };
    if seconds < 0 || !(0..1_000_000_000).contains(&nanoseconds) {
        return Err(EINVAL);
    }
    let seconds = u64::try_from(seconds).map_err(|_| EINVAL)?;
    let nanoseconds = u64::try_from(nanoseconds).map_err(|_| EINVAL)?;
    let total = seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
        .ok_or(EINVAL)?;
    Ok(Some(total))
}

/// `exit_group` and `exit`: leave the program and report the status.
fn sys_exit(status: u64) -> ! {
    // SAFETY: the assembly return path restores the kernel frame saved by the
    // transition into user mode. The program is never resumed.
    unsafe { madrust_user_return(status & 0xff) }
}

/// `getcwd`: the program lives at the root of an empty file system.
fn sys_getcwd(buffer: u64, size: u64) -> i64 {
    if size < 2 {
        return ERANGE;
    }
    if !write_user_bytes(buffer, b"/\0") {
        return EFAULT;
    }
    2
}

/// `pipe2`: create an anonymous pipe and store the two descriptors.
fn sys_pipe2(address: u64) -> i64 {
    if user_range(address, 8).is_none() {
        return EFAULT;
    }
    match fd::create_pipe() {
        Ok((read, write)) => {
            let mut block = [0u8; 8];
            block[0..4].copy_from_slice(&(read as u32).to_le_bytes());
            block[4..8].copy_from_slice(&(write as u32).to_le_bytes());
            if write_user_bytes(address, &block) {
                0
            } else {
                EFAULT
            }
        }
        Err(error) => error,
    }
}

/// `kill`: only the single process of this kernel exists.
fn sys_kill(pid: u64) -> i64 {
    if pid == 1 || pid == 0 {
        0
    } else {
        ESRCH
    }
}

/// `tgkill`: only thread one of process one exists.
fn sys_tgkill(tgid: u64, tid: u64) -> i64 {
    if tgid == 1 && tid == 1 {
        0
    } else {
        ESRCH
    }
}

/// `nanosleep` and `clock_nanosleep`: wait for the requested duration.
fn sys_nanosleep(request: u64, remaining: u64) -> i64 {
    let requested = match user_timeout(request) {
        Ok(Some(nanoseconds)) => nanoseconds,
        Ok(None) => return EINVAL,
        Err(error) => return error,
    };
    if remaining != 0 && !write_user_bytes(remaining, &[0u8; TIME_SIZE]) {
        return EFAULT;
    }
    sleep_nanoseconds(requested);
    0
}

/// Sleep for `nanoseconds` with interrupts enabled.
///
/// A system call arrives with interrupts masked, but the CPU has to take timer
/// interrupts for this to finish, so the flag is set for the wait and cleared
/// again before the dispatcher returns. The interrupt frame is pushed on the
/// current kernel stack, which is the stack the call already runs on.
fn sleep_nanoseconds(nanoseconds: u64) {
    if nanoseconds == 0 {
        return;
    }
    let tick = 1_000_000_000 / pit::TICK_HZ;
    let mut ticks = nanoseconds / tick;
    if !nanoseconds.is_multiple_of(tick) {
        ticks += 1;
    }
    let target = pit::ticks() + ticks;
    cpu::enable_interrupts();
    while pit::ticks() < target {
        cpu::hlt();
    }
    cpu::disable_interrupts();
}

/// `getrusage`: report zeros, because no time is accounted per resource.
fn sys_getrusage(address: u64) -> i64 {
    if address == 0 {
        return 0;
    }
    if write_user_bytes(address, &[0u8; 144]) {
        0
    } else {
        EFAULT
    }
}

/// Validate a user range and borrow it as bytes.
fn user_bytes(address: u64, length: u64) -> Option<&'static [u8]> {
    let length = user_range(address, length)?;
    if length == 0 {
        return Some(&[]);
    }
    // SAFETY: the range was checked to lie inside the user half and every page
    // of it is mapped. The user program of this milestone owns it. The kernel
    // runs at CPL 0 without SMAP, so it may read user pages. A concurrent user
    // write could change the bytes while a call reads them; nothing else in
    // the kernel reads that memory.
    Some(unsafe { core::slice::from_raw_parts(address as *const u8, length) })
}

/// Check that `[address, address + length)` is mapped user memory.
///
/// Returns the length as a `usize`, or `None` when the range is empty, crosses
/// the end of the user half, starts at the null page or touches a page with no
/// translation. Rejecting the range here keeps a bad pointer from becoming a
/// kernel page fault.
fn user_range(address: u64, length: u64) -> Option<usize> {
    if length == 0 {
        return Some(0);
    }
    let end = address.checked_add(length)?;
    if address < PAGE_SIZE || end > USER_SPACE_END {
        return None;
    }
    let mut page = address & !(PAGE_SIZE - 1);
    while page < end {
        paging::frame_at(page)?;
        page += PAGE_SIZE;
    }
    usize::try_from(length).ok()
}

/// Read one value of type `T` from user memory.
fn read_user<T: Copy>(address: u64) -> Option<T> {
    let size = size_of::<T>() as u64;
    user_range(address, size)?;
    // SAFETY: the range was validated as mapped user memory and `T` is a plain
    // old data type, so an unaligned read from the user address is sound.
    Some(unsafe { (address as *const T).read_unaligned() })
}

/// Read one 32-bit futex word from user memory.
fn read_user_u32(address: u64) -> Option<u32> {
    read_user::<u32>(address)
}

/// Copy `bytes` into a user buffer, if the whole range is mapped.
fn write_user_bytes(address: u64, bytes: &[u8]) -> bool {
    let Some(length) = user_range(address, bytes.len() as u64) else {
        return false;
    };
    if length == 0 {
        return true;
    }
    // SAFETY: the destination range was checked to lie inside the user half and
    // every page of it is mapped, the source is a kernel buffer, and the two do
    // not overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len());
    }
    true
}
