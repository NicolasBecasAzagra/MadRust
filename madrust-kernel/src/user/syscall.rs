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
//! * Memory: `brk`, `mmap`, `munmap`, `mprotect`, `madvise`, `mlock` and
//!   `munlock`, with the implementation in [`super::vm`]. Anonymous mappings
//!   reserve address space and fault in their frames on first touch; `mremap`
//!   is not supported and reports `EINVAL`.
//! * Threads and signals: `arch_prctl` (thread local storage), the signal
//!   calls, `futex` (with the wait queue in [`super::futex`]) and `rseq`.
//! * Input and output: the descriptor calls, `write`, `read`, `lseek`,
//!   `uname`, the clocks, `getrandom`, `nanosleep` and `getrusage`.
//!
//! `openat` and `newfstatat` reach the initramfs archive through
//! [`super::initramfs`]: the archive is the root file system, read only, and a
//! user program can open and read any entry. `statx`, `getdents64` and
//! `readlink` are still unimplemented because nothing in the acceptance boot
//! needs directory enumeration or links.

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
const SYS_OPEN: u64 = 2;
/// `write` system call number.
const SYS_WRITE: u64 = 1;
/// `close` system call number.
const SYS_CLOSE: u64 = 3;
/// `fstat` system call number.
const SYS_FSTAT: u64 = 5;
/// `poll` system call number.
const SYS_POLL: u64 = 7;
/// `lseek` system call number.
const SYS_LSEEK: u64 = 8;
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
/// `clone` system call number.
const SYS_CLONE: u64 = 56;
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
/// `mlock` system call number.
const SYS_MLOCK: u64 = 149;
/// `munlock` system call number.
const SYS_MUNLOCK: u64 = 150;
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

/// `AT_FDCWD`: resolve a path against the process working directory.
///
/// The kernel has no working directory, so this value names the archive root.
const AT_FDCWD: u64 = -100i64 as u64;
/// `AT_SYMLINK_NOFOLLOW`: `newfstatat` must not follow a symbolic link.
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
/// `AT_EMPTY_PATH`: `newfstatat` may describe the descriptor itself.
const AT_EMPTY_PATH: u64 = 0x1000;

/// Longest path a system call accepts, including the terminating NUL.
const PATH_MAX: usize = 4096;

/// Stack limit reported to the program, 8 MiB.
const STACK_LIMIT: u64 = 8 * 1024 * 1024;
/// Open file limit reported to the program.
const NOFILE_LIMIT: u64 = 1024;

/// Share the address space with the parent.
const CLONE_VM: u64 = 0x0000_0100;
/// Share the file system information.
const CLONE_FS: u64 = 0x0000_0200;
/// Share the descriptor table.
const CLONE_FILES: u64 = 0x0000_0400;
/// Share the signal handlers.
const CLONE_SIGHAND: u64 = 0x0000_0800;
/// Put the child in the same thread group.
const CLONE_THREAD: u64 = 0x0001_0000;
/// Share the System V semaphore undo list.
const CLONE_SYSVSEM: u64 = 0x0004_0000;
/// Store the child's thread local storage base from the fifth argument.
const CLONE_SETTLS: u64 = 0x0008_0000;
/// Clear `child_tid` and wake its futex when the child exits.
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;

/// Flags this kernel implements for `clone`.
const CLONE_SUPPORTED: u64 = CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_THREAD
    | CLONE_SYSVSEM
    | CLONE_SETTLS
    | CLONE_CHILD_CLEARTID;

/// Number of bytes in each `utsname` field, including the terminating NUL.
const UTS_FIELD: usize = 65;

/// Size of `struct timespec` and `struct timeval`.
const TIME_SIZE: usize = 16;

/// Number of bytes `getrandom` writes per pseudo random draw.
const RANDOM_CHUNK: usize = 16;

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
    let entry = madrust_syscall_entry as *const () as u64;

    // STAR[47:32] is the kernel code selector and STAR[63:48] the base the
    // SYSRET selectors are derived from: code is base + 16 and stack is
    // base + 8. With the ring 3 data selector at 0x23 the base is the ring 3
    // code selector 0x1B, so SYSRET returns to code 0x2B and stack 0x23.
    let star = (u64::from(CODE_SELECTOR) << 32) | (u64::from(USER_CODE_SELECTOR) << 48);

    let efer = cpu::read_msr(MSR_EFER);
    // SAFETY: EFER exists on every x86_64 CPU that has SYSCALL, and only the
    // system call enable bit is added. STAR, LSTAR and SFMASK exist on the same
    // CPUs and receive values built from installed descriptors and the stub
    // address.
    unsafe {
        cpu::write_msr(MSR_EFER, efer | EFER_SYSCALL_ENABLE);
        cpu::write_msr(MSR_STAR, star);
        cpu::write_msr(MSR_LSTAR, entry);
        cpu::write_msr(MSR_SFMASK, RFLAGS_INTERRUPT_FLAG);
    }

    // The boot thread starts without thread local storage; `arch_prctl` moves
    // the base once the program asks for it.
    load_fs_base(0);
}

/// Address of the per-CPU block, for a new user thread's initial GS state.
pub(crate) fn per_cpu_address() -> u64 {
    crate::percpu::this().base()
}

/// User stack pointer the stub stored at `GS:0` on entry.
pub(crate) fn user_stack() -> u64 {
    // SAFETY: the stub is the only writer, it runs on this CPU with interrupts
    // masked, and the caller holds the same exclusion.
    unsafe { *crate::percpu::this().user_stack.get() }
}

/// Put a saved user stack pointer back into the `GS:0` slot.
///
/// The scheduler calls this before switching to a thread that was inside a
/// system call, because the slot belongs to the processor, not to the thread.
pub(crate) fn install_user_stack(stack: u64) {
    // SAFETY: the caller runs with interrupts disabled, so no other thread can
    // enter a system call and race this write on the same CPU.
    unsafe {
        *crate::percpu::this().user_stack.get() = stack;
    }
}

/// Load a thread local storage base into `IA32_FS_BASE`.
///
/// The caller owns the MSR: the scheduler calls this when it switches to a
/// user thread, and `arch_prctl` when the running thread changes its own base.
pub(crate) fn load_fs_base(base: u64) {
    // SAFETY: FS_BASE exists on every x86_64 CPU and the scheduler and
    // `arch_prctl` only pass user addresses or the initial zero.
    unsafe { cpu::write_msr(MSR_FS_BASE, base) };
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
    // The stub left the user stack pointer in the per-CPU slot. Copy it into
    // the thread control block now, before any call can block and let another
    // thread overwrite the shared slot.
    if let Some(id) = crate::sched::current_id() {
        crate::thread::set_user_stack(id, user_stack());
    }

    match number {
        SYS_READ => sys_read(argument1, argument2, argument3),
        SYS_WRITE => sys_write(argument1, argument2, argument3),
        SYS_OPEN => sys_open(argument1, argument2),
        SYS_OPENAT => sys_openat(argument1, argument2, argument3),
        SYS_CLOSE => fd::close(argument1),
        SYS_FSTAT => sys_fstat(argument1, argument2),
        SYS_LSEEK => fd::seek(argument1, argument2 as i64, argument3),
        SYS_NEWFSTATAT => sys_newfstatat(argument1, argument2, argument3, argument4),
        // A call that is not implemented answers ENOSYS, which is what tells
        // the standard library to try the older call. ENOENT would say that
        // the file does not exist, and a program that asks for the metadata
        // of a file that is there would believe it is gone.
        SYS_STATX => ENOSYS,
        SYS_GETDENTS64 => ENOSYS,
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
        SYS_MADVISE => result(vm::madvise(argument1, argument2, argument3)),
        SYS_MLOCK => result(vm::mlock(argument1, argument2)),
        SYS_MUNLOCK => result(vm::munlock(argument1, argument2)),
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
        SYS_CLONE => sys_clone(argument1, argument2, argument3, argument4, argument5),
        SYS_EXIT | SYS_EXIT_GROUP => sys_exit(argument1),
        SYS_GETPID => 1,
        SYS_GETTID => current_tid(),
        SYS_GETPPID => 0,
        SYS_GETUID | SYS_GETEUID | SYS_GETGID | SYS_GETEGID => 0,
        SYS_READLINK => ENOSYS,
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

/// `open`: open one entry of the initramfs archive.
///
/// The old call takes the path and the flags, and the working directory is
/// the root of the archive. The mode is ignored: nothing can be created.
fn sys_open(path: u64, flags: u64) -> i64 {
    let result = sys_openat(AT_FDCWD, path, flags);
    if result < 0 {
        crate::kprintln!("open failed: number=2 path={path:#x} flags={flags:#x} error={result}");
    }
    result
}

/// `openat`: open one entry of the initramfs archive.
///
/// Only `AT_FDCWD` is meaningful, because the working directory is the archive
/// root. The mode argument is ignored: nothing can be created.
///
/// The old `open` call is the same one with the working directory, and a
/// program that uses the standard library asks for it: without it the call
/// answers ENOSYS and the program cannot read a file.
fn sys_openat(dirfd: u64, address: u64, flags: u64) -> i64 {
    if dirfd != AT_FDCWD {
        crate::kprintln!("openat refused: dirfd={dirfd:#x} path={address:#x}");
        return EINVAL;
    }
    let Some(path) = user_c_string(address) else {
        crate::kprintln!("openat refused: path={address:#x} is not readable");
        return EFAULT;
    };
    let result = fd::open(path, flags);
    if result < 0 {
        // The name is in the log: a program that opens a file that is there
        // and does not find it needs to know which name it asked for.
        crate::kprintln!(
            "open failed: number=257 address={address:#x} error={result} name={}",
            core::str::from_utf8(path).unwrap_or("<not utf8>")
        );
    }
    result
}

/// `newfstatat`: describe one entry of the initramfs archive.
fn sys_newfstatat(dirfd: u64, path: u64, address: u64, flags: u64) -> i64 {
    if dirfd != AT_FDCWD {
        return EINVAL;
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return EINVAL;
    }
    let Some(path) = user_c_string(path) else {
        return EFAULT;
    };
    match fd::stat_path(path) {
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

/// `read`: take bytes from a descriptor into the user buffer.
///
/// The destination is the user buffer itself, not a kernel bounce buffer. The
/// kernel and the program share one address space, and a file entry already
/// borrows the archive module memory, so the copy goes straight from the
/// reserved archive to the program.
fn sys_read(descriptor: u64, address: u64, length: u64) -> i64 {
    let Some(length) = user_range_faulted(address, length) else {
        return EFAULT;
    };
    if length == 0 {
        return 0;
    }
    // SAFETY: `user_range` checked that every page of the range is mapped in
    // the user half. The program owns the buffer for the duration of the call
    // and the kernel holds no other reference to it, so the mutable slice is
    // unique while `fd::read` fills it.
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    fd::read(descriptor, buffer)
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
        Err(error) => {
            // The program only sees that the mapping failed. The reason is in
            // the log, because an allocation that fails with no explanation
            // is the hardest kind of failure to fix.
            crate::kprintln!(
                "mmap failed: address={address:#x} length={length:#x} flags={flags:#x} error={error} free_frames={}",
                crate::memory::with(|manager| manager.free_frames()).unwrap_or(0)
            );
            let (in_use, capacity) = crate::user::mappings::usage();
            crate::kprintln!(
                "mmap failed: mappings in use={in_use} of {capacity}"
            );
            error
        }
    }
}

/// `mremap`: resize a mapping, which this kernel does not support.
///
/// [`vm::mremap`] reports `EINVAL`; the wrapper only adapts the return type.
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
            if let Some(id) = crate::sched::current_id() {
                crate::thread::set_fs_base(id, address);
            }
            // SAFETY: FS_BASE exists on every x86_64 CPU. The value is a user
            // address, so the kernel keeps its own segment bases unchanged.
            load_fs_base(address);
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

/// Seconds and nanoseconds since boot from the wall clock.
fn clock_now() -> (u64, u64) {
    crate::clock::now()
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

/// `clone`: create a thread that shares the address space of the caller.
///
/// The six sharing flags are required together; a call without them would be a
/// process, which this milestone does not implement. The child resumes at the
/// `syscall` return address of the parent with RAX zero, exactly like Linux, and
/// the parent receives the child thread identifier.
fn sys_clone(flags: u64, child_stack: u64, _parent_tid: u64, child_tid: u64, tls: u64) -> i64 {
    if flags & !CLONE_SUPPORTED != 0 {
        return EINVAL;
    }
    if flags & (CLONE_VM | CLONE_THREAD) != (CLONE_VM | CLONE_THREAD) {
        return EINVAL;
    }
    if child_stack == 0 || child_stack >= USER_SPACE_END {
        return EINVAL;
    }
    let Some(current) = crate::sched::current_id() else {
        return EINVAL;
    };

    // `madrust_syscall_entry` published the frame it saved, so the parent's
    // registers are still available while the dispatcher runs.
    let frame = current_frame();
    let saved = |index: usize| -> u64 {
        // SAFETY: the stub wrote a pointer to its own frame, the frame is on
        // the current kernel stack, and interrupts stay masked for the whole
        // system call. Every index below is inside the fifteen pushed words.
        unsafe { frame.add(index).read_volatile() }
    };

    let return_address = saved(14);
    if !(2 * PAGE_SIZE..USER_SPACE_END).contains(&return_address) {
        return EINVAL;
    }

    let fs_base = if flags & CLONE_SETTLS != 0 {
        if tls >= USER_SPACE_END {
            return EPERM;
        }
        tls
    } else {
        crate::thread::fs_base(current).unwrap_or(0)
    };

    let start = crate::thread::UserStart {
        rip: return_address,
        rsp: child_stack,
        // Keep the parent flags, force interrupts on so the timer can preempt
        // the new thread, and clear the bits a ring 3 context must not carry
        // (IOPL, NT and VM).
        rflags: (saved(13) | RFLAGS_INTERRUPT_FLAG) & !(0x3000 | 0x4000 | 0x2_0000),
        rax: 0,
        rbx: saved(11),
        rbp: saved(7),
        r12: saved(3),
        r13: saved(2),
        r14: saved(1),
        r15: saved(0),
        rdi: saved(8),
        rsi: saved(9),
        rdx: saved(10),
        r8: saved(6),
        r9: saved(5),
        r10: saved(4),
    };

    let clear_child_tid = if flags & CLONE_CHILD_CLEARTID != 0 {
        if child_tid >= USER_SPACE_END {
            return EFAULT;
        }
        child_tid
    } else {
        0
    };

    match crate::sched::spawn_user("user", start, fs_base, clear_child_tid, per_cpu_address()) {
        Ok(id) => i64::try_from(id + 1).unwrap_or(i64::MAX),
        Err(error) => {
            // The reason belongs in the log: a bare EAGAIN sends the reader
            // looking for the problem in the program instead of in the kernel.
            crate::kprintln!("[user] clone refused: {error:?}");
            EAGAIN
        }
    }
}

/// Thread identifier of the calling thread, one based like Linux.
fn current_tid() -> i64 {
    match crate::sched::current_id() {
        Some(id) => i64::try_from(id + 1).unwrap_or(i64::MAX),
        None => 0,
    }
}

/// Pointer to the register frame the system call stub saved.
fn current_frame() -> *const u64 {
    // SAFETY: `madrust_syscall_entry` stores the attacker cannot reach the
    // pointer before the dispatcher runs, interrupts are masked, and the frame
    // lives on the current kernel stack. No other path mutates the slot during
    // the call.
    unsafe { *crate::percpu::this().user_frame.get() as *const u64 }
}

/// `exit_group` and `exit`: leave the program and report the status.
fn sys_exit(status: u64) -> ! {
    match crate::sched::current_id() {
        // The boot thread owns the frame `madrust_enter_user_mode` saved; its
        // exit returns to the kernel boot path.
        Some(0) | None => {
            // SAFETY: the assembly return path restores the kernel frame saved
            // by the transition into user mode. The program is never resumed.
            unsafe { madrust_user_return(status & 0xff) }
        }
        Some(id) => {
            // A thread created by `clone` wakes whoever waits on its
            // `child_tid` futex, then retires without touching the boot frame.
            let address = crate::thread::clear_child_tid(id);
            if address != 0 && write_user_bytes(address, &0u32.to_le_bytes()) {
                futex::wake(address, u64::MAX);
            }
            crate::sched::retire_current()
        }
    }
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
    let length = user_range_faulted(address, length)?;
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

/// Borrow a NUL terminated path from user memory.
///
/// Every byte is validated against the mapped user range, so a bad pointer or a
/// string that runs off the mapped pages reports `None` instead of faulting in
/// the kernel. The length bound keeps a missing terminator from walking the
/// whole address space.
fn user_c_string(address: u64) -> Option<&'static [u8]> {
    let mut length = 0u64;
    loop {
        if length >= PATH_MAX as u64 {
            return None;
        }
        let position = address.checked_add(length)?;
        user_range(position, 1)?;
        // SAFETY: the single byte at `position` was checked as mapped user
        // memory above, and the program only changes it concurrently if it
        // races its own path buffer.
        if unsafe { (position as *const u8).read() } == 0 {
            break;
        }
        length += 1;
    }
    let length = usize::try_from(length).ok()?;
    if length == 0 {
        return Some(&[]);
    }
    // SAFETY: the bytes from `address` to the NUL were checked one by one as
    // mapped user memory, and the slice excludes the terminator.
    Some(unsafe { core::slice::from_raw_parts(address as *const u8, length) })
}

/// Check that `[address, address + length)` is mapped user memory.
///
/// Returns the length as a `usize`, or `None` when the range is empty, crosses
/// the end of the user half, starts at the null page or touches a page with no
/// translation. Rejecting the range here keeps a bad pointer from becoming a
/// kernel page fault.
/// Check a range of user memory and bring in the pages that are missing.
///
/// A program that reads into a buffer it has just allocated asks for pages
/// that the kernel has not mapped yet: the demand paging maps them when the
/// program touches them, and a system call that writes into the buffer has to
/// do the same. A range that cannot be brought in is refused.
fn user_range_faulted(address: u64, length: u64) -> Option<usize> {
    if length == 0 {
        return Some(0);
    }
    let end = address.checked_add(length)?;
    if address < PAGE_SIZE || end > USER_SPACE_END {
        return None;
    }
    let mut page = address & !(PAGE_SIZE - 1);
    while page < end {
        if paging::frame_at(page).is_none() {
            // The page is missing: the demand paging maps it when the program
            // touches it, and the call is the touch.
            if vm::handle_page_fault(page, vm::PAGE_FAULT_WRITE) != vm::PageFaultOutcome::Resolved {
                return None;
            }
        }
        page += PAGE_SIZE;
    }
    usize::try_from(length).ok()
}

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
