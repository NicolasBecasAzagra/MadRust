// SPDX-License-Identifier: GPL-2.0-only
//! MadRust benchmark workload.
//!
//! One static binary for the Linux musl target holds every measurement, so the
//! exact same bytes run on MadRust and on mainline Linux. It uses only system
//! calls whose Linux semantics both kernels share: `getpid`, `mmap`, `munmap`,
//! `futex`, `clone` and `exit`. `std::time` supplies the clock and `std::io`
//! the output; the thread entry is a direct `clone` call because the standard
//! library uses a flag combination MadRust does not implement.
//!
//! The binary prints one `RESULT` line per sample so the harness records every
//! iteration, including the ones that failed, and `BENCH DONE` at the end. A
//! failure prints the negated Linux error number instead of a value.
//!
//! ## Workloads
//!
//! * `syscall`: a tight `getpid` loop, reported in ns per call.
//! * `mmap`: map an anonymous region, touch every page, unmap, reported in ns
//!   per MiB.
//! * `futex`: two threads ping-pong through one futex word, reported in ns per
//!   round trip.
//! * `threads`: four threads do a fixed amount of arithmetic, reported as the
//!   wall clock time of the whole spawn, run and join sequence.
//! * `weights`: map a large fixed region and touch every page, reported as ns
//!   per MiB and as the slowest fault per page in ns. `weights-huge` runs the
//!   same loop after `madvise(MADV_HUGEPAGE)`, so the 4 KiB and the 2 MiB
//!   fault regimes can be compared.
//! * `file`: open `/bench` from the initramfs, stat it, mmap it read only,
//!   read a byte from the mapping, read it and seek it, then open a path that
//!   does not exist. It runs first and prints exact lines, so the serial log
//!   proves the read only file system and the file backed mapping end to end.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::hint::black_box;
use std::time::Instant;

/// Samples per workload.
const ITERATIONS: usize = 30;

/// `getpid` calls per syscall sample.
const SYSCALL_OPS: u64 = 20_000;

/// Bytes mapped and touched per mmap sample, four MiB.
const MMAP_BYTES: usize = 4 * 1024 * 1024;

/// Ping-pong round trips per futex sample.
const FUTEX_ROUNDTRIPS: u32 = 8;

/// Threads in the thread sample.
const THREAD_COUNT: usize = 4;

/// Arithmetic iterations per worker thread.
const THREAD_WORK: u64 = 200_000;

/// Bytes mapped for one user thread stack.
const STACK_BYTES: usize = 256 * 1024;

/// Bytes the weights workload maps and touches per sample.
///
/// The guest has 512 MiB of RAM, so the region takes half of it: large enough
/// that the fault in path dominates the measurement, small enough that both
/// kernels can hold it without exhausting memory.
const WEIGHTS_BYTES: usize = if cfg!(feature = "small") {
    32 * 1024 * 1024
} else {
    256 * 1024 * 1024
};

/// Samples of the weights workload.
///
/// One sample touches 65 536 pages, so a sample costs far more than the other
/// workloads. Five samples finish in a few seconds under emulation.
const WEIGHTS_ITERATIONS: usize = 5;

/// Samples of the large page weights workload.
///
/// A 2 MiB fault is rare enough that one host preemption can dominate the
/// slowest sample, so the huge run takes twice as many samples.
const WEIGHTS_HUGE_ITERATIONS: usize = 10;

/// Fixed base address of the weights mapping.
///
/// `MAP_FIXED` puts every sample at the same address, so the two kernels and
/// consecutive samples see the same virtual range.
const WEIGHTS_BASE: u64 = 0x0000_0010_0000_0000;

/// How long the timestamp calibration runs.
const CALIBRATION_MICROS: u64 = 5_000;

/// Page size the workloads assume; both kernels use 4 KiB pages.
const PAGE_SIZE: usize = 4096;

/// Size of `struct stat` on x86-64.
const STAT_SIZE: usize = 144;

/// Path the file check opens; the initramfs stores the workload there.
const BENCH_PATH: &[u8] = b"/bench\0";
/// Path that must not exist.
const MISSING_PATH: &[u8] = b"/missing\0";
/// `AT_FDCWD`: resolve the path against the working directory.
const AT_FDCWD: u64 = -100i64 as u64;
/// `O_RDONLY`.
const O_RDONLY: u64 = 0;
/// `SEEK_END`.
const SEEK_END: u64 = 2;

const SYS_READ: u64 = 0;
const SYS_CLOSE: u64 = 3;
const SYS_FSTAT: u64 = 5;
const SYS_LSEEK: u64 = 8;
const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
const SYS_MADVISE: u64 = 28;
const SYS_GETPID: u64 = 39;
const SYS_EXIT: u64 = 60;
const SYS_FUTEX: u64 = 202;
const SYS_OPENAT: u64 = 257;
const SYS_NEWFSTATAT: u64 = 262;
const SYS_REBOOT: u64 = 169;

/// `LINUX_REBOOT_MAGIC1`: the first word the kernel checks.
const LINUX_REBOOT_MAGIC1: u64 = 0x0123_4567;
/// `LINUX_REBOOT_MAGIC2`: the second word the kernel checks.
const LINUX_REBOOT_MAGIC2: u64 = 0xCDEF_0123;
/// `LINUX_REBOOT_CMD_POWER_OFF`: the command that stops the machine.
const LINUX_REBOOT_CMD_POWER_OFF: u64 = 0x4321_FEDC;

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_PRIVATE: u64 = 2;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;

const MADV_NORMAL: u64 = 0;
const MADV_HUGEPAGE: u64 = 14;

/// Bytes one 2 MiB large page covers.
const LARGE_PAGE_BYTES: usize = 2 * 1024 * 1024;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;

const CLONE_VM: u64 = 0x0000_0100;
const CLONE_FS: u64 = 0x0000_0200;
const CLONE_FILES: u64 = 0x0000_0400;
const CLONE_SIGHAND: u64 = 0x0000_0800;
const CLONE_THREAD: u64 = 0x0001_0000;
const CLONE_SYSVSEM: u64 = 0x0004_0000;
const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;

/// Flags that make `clone` create a thread that shares the address space.
const THREAD_FLAGS: u64 = CLONE_VM
    | CLONE_FS
    | CLONE_FILES
    | CLONE_SIGHAND
    | CLONE_THREAD
    | CLONE_SYSVSEM
    | CLONE_CHILD_CLEARTID;

/// Futex word the two ping-pong threads share.
static FUTEX_WORD: AtomicU32 = AtomicU32::new(0);
/// Thread identifier slot the futex worker clears when it exits.
static FUTEX_DONE: AtomicU32 = AtomicU32::new(1);

/// One completion word per worker of the thread sample.
static THREAD_DONE: [AtomicU32; THREAD_COUNT] = [
    AtomicU32::new(1),
    AtomicU32::new(1),
    AtomicU32::new(1),
    AtomicU32::new(1),
];

/// Keeps the arithmetic of the workers observable.
static THREAD_ACC: AtomicU64 = AtomicU64::new(0);

extern "C" {
    /// Create a thread and continue on both sides.
    ///
    /// The child returns from `syscall` on the new stack, so the assembly
    /// branches on RAX and calls `entry(argument)` instead of returning through
    /// a parent frame that is not on that stack.
    fn bench_clone(
        flags: u64,
        stack_top: u64,
        entry: extern "C" fn(*mut u8) -> !,
        argument: *mut u8,
        child_tid: *mut u32,
    ) -> i64;
}

core::arch::global_asm!(
    r#"
    .text
    .global bench_clone
    .type bench_clone, @function
bench_clone:
    pushq %r12
    pushq %r13
    movq %rdx, %r12            /* entry function */
    movq %rcx, %r13            /* entry argument */
    movq %r8, %r10             /* child_tid, the fourth clone argument */
    xorl %r8d, %r8d            /* tls: inherit the parent FS base */
    xorl %edx, %edx            /* parent_tid: unused */
    movl $56, %eax             /* SYS_clone */
    syscall
    testq %rax, %rax
    jne 1f

    /* Child: RAX is zero. Run the entry on the new stack and never return. */
    movq %r13, %rdi
    andq $-16, %rsp
    callq *%r12
    movl $60, %eax             /* SYS_exit, if the entry ever returns */
    xorl %edi, %edi
    syscall
    hlt

1:  /* Parent: RAX holds the child thread identifier. */
    popq %r13
    popq %r12
    ret
"#,
    options(att_syntax)
);

/// Raw `syscall` with one argument.
///
/// # Safety
///
/// The caller must pass a system call and arguments valid for the running
/// kernel. The assembly only issues the instruction.
unsafe fn syscall1(number: u64, argument1: u64) -> i64 {
    let result: i64;
    // SAFETY: the caller upholds the contract above. The clobbered registers
    // are the ones the syscall instruction always destroys.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") argument1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Raw `syscall` with three arguments.
///
/// # Safety
///
/// See [`syscall1`].
unsafe fn syscall3(number: u64, argument1: u64, argument2: u64, argument3: u64) -> i64 {
    let result: i64;
    // SAFETY: the caller upholds the contract above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") argument1,
            in("rsi") argument2,
            in("rdx") argument3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Raw `syscall` with four arguments.
///
/// # Safety
///
/// See [`syscall1`].
unsafe fn syscall4(
    number: u64,
    argument1: u64,
    argument2: u64,
    argument3: u64,
    argument4: u64,
) -> i64 {
    let result: i64;
    // SAFETY: the caller upholds the contract above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") argument1,
            in("rsi") argument2,
            in("rdx") argument3,
            in("r10") argument4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Raw `syscall` with six arguments.
///
/// # Safety
///
/// See [`syscall1`].
#[allow(clippy::too_many_arguments)]
unsafe fn syscall6(
    number: u64,
    argument1: u64,
    argument2: u64,
    argument3: u64,
    argument4: u64,
    argument5: u64,
    argument6: u64,
) -> i64 {
    let result: i64;
    // SAFETY: the caller upholds the contract above.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as i64 => result,
            in("rdi") argument1,
            in("rsi") argument2,
            in("rdx") argument3,
            in("r10") argument4,
            in("r8") argument5,
            in("r9") argument6,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Ask the machine to go down, so the kernel does not panic when the last
/// process leaves.
///
/// The call is `reboot(magic1, magic2, cmd)`: the two magic words are the
/// first two arguments and the command is the third. The kernel answers
/// `-EINVAL` when the order is wrong, so the order is not a detail.
///
/// Mainline Linux panics with "Attempted to kill init!" when PID 1 exits, and
/// the workload is PID 1 there. Asking the kernel to power the machine off
/// ends the run without that panic. A kernel that does not know the call
/// answers with an error and the process leaves the ordinary way, which is
/// what MadRust does.
fn power_off() {
    // SAFETY: reboot takes three arguments. On a kernel that accepts it the
    // call never returns; on a kernel that refuses it, the result is an error
    // number and the caller continues.
    unsafe {
        syscall3(
            SYS_REBOOT,
            LINUX_REBOOT_MAGIC1,
            LINUX_REBOOT_MAGIC2,
            LINUX_REBOOT_CMD_POWER_OFF,
        );
    }
}

/// Terminate only the calling thread, like `SYS_exit` on Linux.
fn exit_thread() -> ! {
    // SAFETY: SYS_exit takes one status argument and never returns.
    unsafe { syscall1(SYS_EXIT, 0) };
    // A kernel that returns from exit would otherwise run off the thread.
    loop {
        core::hint::spin_loop();
    }
}

/// Map `length` anonymous bytes and return the base address.
fn map(length: usize) -> Result<u64, i64> {
    // SAFETY: mmap takes six arguments; the descriptor is -1 because the
    // mapping is anonymous.
    let address = unsafe {
        syscall6(
            SYS_MMAP,
            0,
            length as u64,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            u64::MAX,
            0,
        )
    };
    if address < 0 {
        Err(address)
    } else {
        Ok(address as u64)
    }
}

/// Map `length` anonymous bytes at a fixed address.
fn map_at(address: u64, length: usize) -> Result<u64, i64> {
    // SAFETY: mmap takes six arguments; the descriptor is -1 because the
    // mapping is anonymous, and MAP_FIXED places it at `address`.
    let mapped = unsafe {
        syscall6(
            SYS_MMAP,
            address,
            length as u64,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED,
            u64::MAX,
            0,
        )
    };
    if mapped < 0 {
        Err(mapped)
    } else {
        Ok(mapped as u64)
    }
}

/// Remove a mapping created by [`map`].
fn unmap(address: u64, length: usize) {
    // SAFETY: the range came from `map` with the same length.
    let _ = unsafe { syscall4(SYS_MUNMAP, address, length as u64, 0, 0) };
}

/// Sleep until the word at `address` no longer holds `expected`.
fn futex_wait(address: *const u32, expected: u32) {
    // SAFETY: `address` points at a live shared word; the kernel validates it.
    let _ = unsafe {
        syscall6(
            SYS_FUTEX,
            address as u64,
            FUTEX_WAIT,
            expected as u64,
            0,
            0,
            0,
        )
    };
}

/// Wake one waiter of the word at `address`.
fn futex_wake(address: *const u32) {
    // SAFETY: `address` points at a live shared word; the kernel validates it.
    let _ = unsafe { syscall6(SYS_FUTEX, address as u64, FUTEX_WAKE, 1, 0, 0, 0) };
}

/// Wait until `word` holds `target`.
///
/// A `FUTEX_WAIT` that races with the store returns `EAGAIN`, so the loop only
/// sleeps while the value is still the one it just read.
fn wait_until(word: &AtomicU32, target: u32) {
    while word.load(Ordering::Acquire) != target {
        let current = word.load(Ordering::Acquire);
        futex_wait(word.as_ptr(), current);
    }
}

/// Wait for a thread that clears its completion word on exit.
fn join(done: &AtomicU32) {
    while done.load(Ordering::Acquire) != 0 {
        let current = done.load(Ordering::Acquire);
        futex_wait(done.as_ptr(), current);
    }
}

/// Map a user thread stack and return its base and top.
fn map_stack() -> Result<(u64, u64), i64> {
    let base = map(STACK_BYTES)?;
    Ok((base, base + STACK_BYTES as u64))
}

/// `syscall`: average cost of one `getpid` call.
fn measure_syscall() -> Result<u128, i64> {
    let start = Instant::now();
    for _ in 0..SYSCALL_OPS {
        // SAFETY: getpid takes no arguments and is implemented by both kernels.
        let pid = unsafe { syscall1(SYS_GETPID, 0) };
        black_box(pid);
    }
    let elapsed = start.elapsed().as_nanos();
    Ok(elapsed / u128::from(SYSCALL_OPS))
}

/// `mmap`: cost to map and touch one MiB.
fn measure_mmap() -> Result<u128, i64> {
    let start = Instant::now();
    let base = map(MMAP_BYTES)?;
    let mut offset = 0;
    while offset < MMAP_BYTES {
        // SAFETY: the page was just mapped and the offset stays inside it.
        unsafe { core::ptr::write_volatile((base as usize + offset) as *mut u8, 1) };
        offset += PAGE_SIZE;
    }
    let elapsed = start.elapsed().as_nanos();
    unmap(base, MMAP_BYTES);
    let mib = (MMAP_BYTES / (1024 * 1024)) as u128;
    Ok(elapsed / mib)
}

/// One timestamp counter reading.
///
/// The counter is free in ring 3 and cheap enough to bracket every page, which
/// a clock system call is not on a kernel without a vDSO.
fn ticks() -> u64 {
    // SAFETY: RDTSC is unprivileged on every x86_64 kernel here.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Nanoseconds per timestamp counter tick, measured once per run.
#[derive(Clone, Copy)]
struct TickScale {
    ticks: u64,
    nanos: u128,
}

impl TickScale {
    /// Measure the counter frequency against the monotonic clock.
    fn measure() -> Self {
        let start = Instant::now();
        let first = ticks();
        while start.elapsed().as_micros() < u128::from(CALIBRATION_MICROS) {
            core::hint::spin_loop();
        }
        let last = ticks();
        Self {
            ticks: last.wrapping_sub(first).max(1),
            nanos: start.elapsed().as_nanos().max(1),
        }
    }

    /// Convert a counter delta to nanoseconds.
    fn to_nanos(self, delta: u64) -> u128 {
        u128::from(delta) * self.nanos / u128::from(self.ticks)
    }
}

/// `weights`: map a large region, touch every page, drop it.
///
/// The page fault path is the dominant kernel cost of loading a model. The
/// region stands in for the weights: it is mapped and every 4 KiB page is
/// touched once. `huge` asks the kernel for 2 MiB pages with `madvise` first,
/// so the same loop measures the two fault regimes.
///
/// The slowest value is the slowest fault per page it covers: a 4 KiB fault
/// covers one page, while the first fault of a 2 MiB page covers 512. That
/// makes the two regimes comparable on the same unit. Returns the cost per
/// MiB and the slowest fault per page, both in nanoseconds.
fn measure_weights(scale: TickScale, huge: bool) -> Result<(u128, u128), i64> {
    let start = Instant::now();
    let base = map_at(WEIGHTS_BASE, WEIGHTS_BYTES)?;
    if huge {
        // Probe what the kernel makes of the advice, so a refusal is reported
        // instead of looking like a measurement. MADV_NORMAL is the control:
        // it works on every kernel, so a refusal of MADV_HUGEPAGE alone means
        // the kernel does not implement large pages for this mapping.
        let normal = unsafe { syscall3(SYS_MADVISE, base, WEIGHTS_BYTES as u64, MADV_NORMAL) };
        let huge_advice =
            unsafe { syscall3(SYS_MADVISE, base, WEIGHTS_BYTES as u64, MADV_HUGEPAGE) };
        // A plain line, not a result: this is a probe of the kernel, not a
        // measurement, and the rig counts the result lines.
        println!(
            "ADV madvise-normal={} madvise-hugepage={}",
            normal, huge_advice
        );
    }

    if huge {
        // SAFETY: the range was just mapped and the advice only marks it; the
        // kernel validates the whole range.
        let result = unsafe { syscall3(SYS_MADVISE, base, WEIGHTS_BYTES as u64, MADV_HUGEPAGE) };
        if result < 0 {
            unmap(base, WEIGHTS_BYTES);
            return Err(result);
        }
    }

    let mut slowest = 0u128;
    let mut offset = 0;
    while offset < WEIGHTS_BYTES {
        let before = ticks();
        // SAFETY: the page was just mapped read-write and the offset stays
        // inside the region.
        unsafe {
            let page = (base as usize + offset) as *mut u8;
            let value = core::ptr::read_volatile(page);
            core::ptr::write_volatile(page, value.wrapping_add(1));
        }
        let mut page_nanos = scale.to_nanos(ticks().wrapping_sub(before));
        if huge && offset.is_multiple_of(LARGE_PAGE_BYTES) {
            // A 2 MiB fault covers 512 pages, so charge each of them its share.
            page_nanos /= (LARGE_PAGE_BYTES / PAGE_SIZE) as u128;
        }
        if page_nanos > slowest {
            slowest = page_nanos;
        }
        offset += PAGE_SIZE;
    }

    let elapsed = start.elapsed().as_nanos();
    unmap(base, WEIGHTS_BYTES);
    let mib = (WEIGHTS_BYTES / (1024 * 1024)) as u128;
    Ok((elapsed / mib, slowest))
}

/// Entry of the second futex thread: it answers every round of the parent.
///
/// The first round is a warm up that brings the new thread into ring 3 before
/// the measured ping-pong starts; the parent does not time it.
extern "C" fn futex_worker(_argument: *mut u8) -> ! {
    for round in 0..FUTEX_ROUNDTRIPS + 1 {
        wait_until(&FUTEX_WORD, 2 * round + 1);
        FUTEX_WORD.store(2 * round + 2, Ordering::Release);
        futex_wake(FUTEX_WORD.as_ptr());
    }
    exit_thread()
}

/// `futex`: average round trip between two threads.
fn measure_futex() -> Result<u128, i64> {
    FUTEX_WORD.store(0, Ordering::Release);
    // Keep the child's identifier in a location `clone` can clear on exit.
    FUTEX_DONE.store(1, Ordering::Release);
    let (_base, top) = map_stack()?;

    let child = unsafe {
        bench_clone(
            THREAD_FLAGS,
            top,
            futex_worker,
            core::ptr::null_mut(),
            FUTEX_DONE.as_ptr(),
        )
    };
    if child < 0 {
        unmap(top - STACK_BYTES as u64, STACK_BYTES);
        return Err(child);
    }

    // Warm up: let the new thread reach user mode and answer one round before
    // the clock starts, so the measured cost is the steady state round trip.
    FUTEX_WORD.store(1, Ordering::Release);
    futex_wake(FUTEX_WORD.as_ptr());
    wait_until(&FUTEX_WORD, 2);

    let start = Instant::now();
    for round in 0..FUTEX_ROUNDTRIPS {
        FUTEX_WORD.store(2 * round + 3, Ordering::Release);
        futex_wake(FUTEX_WORD.as_ptr());
        wait_until(&FUTEX_WORD, 2 * round + 4);
    }
    let elapsed = start.elapsed().as_nanos();

    join(&FUTEX_DONE);
    unmap(top - STACK_BYTES as u64, STACK_BYTES);
    Ok(elapsed / u128::from(FUTEX_ROUNDTRIPS))
}

/// Entry of one worker of the thread sample.
extern "C" fn thread_worker(argument: *mut u8) -> ! {
    let index = argument as usize as u64;
    let mut value = index.wrapping_add(1);
    for step in 0..THREAD_WORK {
        value = value
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(step);
    }
    THREAD_ACC.fetch_add(value, Ordering::Relaxed);
    exit_thread()
}

/// `threads`: wall clock time of four threads doing fixed work.
fn measure_threads() -> Result<u128, i64> {
    THREAD_ACC.store(0, Ordering::Release);
    let mut stacks = [(0u64, 0u64); THREAD_COUNT];

    let start = Instant::now();
    for (started, index) in (0..THREAD_COUNT).enumerate() {
        let (base, top) = match map_stack() {
            Ok(stack) => stack,
            Err(error) => {
                for done in &THREAD_DONE[..started] {
                    join(done);
                }
                for (mapped, _) in &stacks[..index] {
                    unmap(*mapped, STACK_BYTES);
                }
                return Err(error);
            }
        };
        stacks[index] = (base, top);
        THREAD_DONE[index].store(1, Ordering::Release);
        let child = unsafe {
            bench_clone(
                THREAD_FLAGS,
                top,
                thread_worker,
                index as *mut u8,
                THREAD_DONE[index].as_ptr(),
            )
        };
        if child < 0 {
            // Wait for the workers that did start, then report the failure.
            for done in &THREAD_DONE[..started] {
                join(done);
            }
            for (base, _) in &stacks[..=index] {
                unmap(*base, STACK_BYTES);
            }
            return Err(child);
        }
    }

    for done in &THREAD_DONE[..THREAD_COUNT] {
        join(done);
    }
    let elapsed = start.elapsed().as_nanos();

    for (base, _) in &stacks {
        unmap(*base, STACK_BYTES);
    }
    Ok(elapsed)
}

/// Print one sample, with the error number when the kernel refused the call.
fn report(name: &str, unit: &str, result: Result<u128, i64>) {
    match result {
        Ok(value) => println!("RESULT {name} {value} {unit}"),
        Err(error) => println!("RESULT {name} ERR {error}"),
    }
}

/// Open a path from the initramfs, or return the negated error number.
fn open(path: &[u8], flags: u64) -> Result<u64, i64> {
    // SAFETY: openat takes four arguments and `path` is a live NUL terminated
    // string; both kernels implement the call.
    let result = unsafe { syscall4(SYS_OPENAT, AT_FDCWD, path.as_ptr() as u64, flags, 0) };
    if result < 0 {
        Err(result)
    } else {
        Ok(result as u64)
    }
}

/// Read `st_size` from an x86-64 `struct stat`.
fn stat_size(block: &[u8; STAT_SIZE]) -> u64 {
    let bytes: [u8; 8] = block[48..56].try_into().unwrap_or_default();
    u64::from_le_bytes(bytes)
}

/// Read `st_mode` from an x86-64 `struct stat`.
fn stat_mode(block: &[u8; STAT_SIZE]) -> u32 {
    let bytes: [u8; 4] = block[24..28].try_into().unwrap_or_default();
    u32::from_le_bytes(bytes)
}

/// Report the size through `newfstatat`, the path based call.
fn report_path_size() {
    let mut block = [0u8; STAT_SIZE];
    // SAFETY: `block` is a live 144-byte buffer and the path string lives for
    // the call; both kernels implement newfstatat.
    let result = unsafe {
        syscall4(
            SYS_NEWFSTATAT,
            AT_FDCWD,
            BENCH_PATH.as_ptr() as u64,
            block.as_mut_ptr() as u64,
            0,
        )
    };
    if result < 0 {
        println!("FILE path-size ERR {result}");
    } else {
        println!("FILE path-size={}", stat_size(&block));
    }
}

/// Map the initramfs file read only and read a byte from the mapping.
///
/// The mapping is file backed: the kernel points the page table entry at the
/// archive bytes instead of copying them, and the first read faults the page
/// in. The size the kernel accepted is part of the serial contract, so the log
/// proves the path end to end.
fn map_file(descriptor: u64, size: u64) {
    // Map every whole page of the file; a partial last page would reach past
    // the file and the kernel refuses that.
    let length = (size as usize) / PAGE_SIZE * PAGE_SIZE;
    if length == 0 {
        println!("FILE mmap=0");
        return;
    }
    // SAFETY: mmap takes six arguments; the descriptor is open read only and
    // the offset is page aligned.
    let mapped = unsafe {
        syscall6(
            SYS_MMAP,
            0,
            length as u64,
            PROT_READ,
            MAP_PRIVATE,
            descriptor,
            0,
        )
    };
    if mapped < 0 {
        println!("FILE mmap ERR {mapped}");
        return;
    }
    // SAFETY: the range was just mapped readable and the first byte is inside
    // it. The read faults the first page in.
    let byte = unsafe { core::ptr::read_volatile(mapped as *const u8) };
    println!("FILE mmap={length}");
    println!("FILE mmap-byte={byte}");
    unmap(mapped as u64, length);
}

/// Read the whole file and report how many bytes the kernel returned.
fn read_file(descriptor: u64, size: u64) {
    let mut buffer = [0u8; PAGE_SIZE];
    let mut total = 0u64;
    loop {
        // SAFETY: the buffer is live for the call and its length is the count
        // passed to the kernel.
        let count = unsafe {
            syscall3(
                SYS_READ,
                descriptor,
                buffer.as_mut_ptr() as u64,
                buffer.len() as u64,
            )
        };
        if count < 0 {
            println!("FILE read ERR {count}");
            return;
        }
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > size {
            break;
        }
    }
    println!("FILE read={total}");

    // SAFETY: lseek takes three arguments and the descriptor is open.
    let end = unsafe { syscall3(SYS_LSEEK, descriptor, 0, SEEK_END) };
    if end < 0 {
        println!("FILE seek ERR {end}");
    } else {
        println!("FILE seek={end}");
    }
}

/// Prove the read only file system end to end.
///
/// The exact lines are part of the serial contract: the workload name, the size
/// from `fstat`, the size from `newfstatat`, the bytes actually read, the end
/// offset from `lseek`, the archive mode with its execute bit, and the `ENOENT`
/// a missing path must return. A path that does not exist must fail, and a
/// successful open would print `FILE missing=UNEXPECTED` instead.
fn check_file() {
    match open(BENCH_PATH, O_RDONLY) {
        Err(error) => println!("FILE size ERR {error}"),
        Ok(descriptor) => {
            let mut block = [0u8; STAT_SIZE];
            // SAFETY: `block` is live for the call; fstat writes at most 144
            // bytes, the size of the buffer.
            let result =
                unsafe { syscall4(SYS_FSTAT, descriptor, block.as_mut_ptr() as u64, 0, 0) };
            if result < 0 {
                println!("FILE size ERR {result}");
            } else {
                let size = stat_size(&block);
                println!("FILE size={size}");
                println!("FILE mode={}", stat_mode(&block));
                report_path_size();
                map_file(descriptor, size);
                read_file(descriptor, size);
            }
            // SAFETY: the descriptor came from open and close takes one
            // argument.
            let _ = unsafe { syscall1(SYS_CLOSE, descriptor) };
        }
    }

    match open(MISSING_PATH, O_RDONLY) {
        Ok(descriptor) => {
            println!("FILE missing=UNEXPECTED");
            // SAFETY: the descriptor came from open and close takes one
            // argument.
            let _ = unsafe { syscall1(SYS_CLOSE, descriptor) };
        }
        Err(error) => println!("FILE missing={error}"),
    }
}

fn main() {
    check_file();
    for _ in 0..ITERATIONS {
        report("syscall", "ns/op", measure_syscall());
    }
    for _ in 0..ITERATIONS {
        report("mmap", "ns/MiB", measure_mmap());
    }
    let scale = TickScale::measure();
    // Large pages first: the 2 MiB blocks need clean free memory, and the
    // 4 KiB run that follows leaves page table pages scattered behind it.
    for _ in 0..WEIGHTS_HUGE_ITERATIONS {
        let result = measure_weights(scale, true);
        report("weights-huge", "ns/MiB", result.map(|sample| sample.0));
        report(
            "weights-huge-tail",
            "ns/page",
            result.map(|sample| sample.1),
        );
    }
    for _ in 0..WEIGHTS_ITERATIONS {
        let result = measure_weights(scale, false);
        report("weights", "ns/MiB", result.map(|sample| sample.0));
        report("weights-tail", "ns/page", result.map(|sample| sample.1));
    }
    for _ in 0..ITERATIONS {
        report("futex", "ns/roundtrip", measure_futex());
    }
    for _ in 0..ITERATIONS {
        report("threads", "ns/total", measure_threads());
    }

    println!("BENCH DONE");
    // The last line has to reach the host before the machine goes down.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    power_off();
    std::process::exit(0);
}
