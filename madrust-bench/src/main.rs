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

/// Page size the workloads assume; both kernels use 4 KiB pages.
const PAGE_SIZE: usize = 4096;

const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
const SYS_GETPID: u64 = 39;
const SYS_EXIT: u64 = 60;
const SYS_FUTEX: u64 = 202;

const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_PRIVATE: u64 = 2;
const MAP_ANONYMOUS: u64 = 0x20;

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

fn main() {
    for _ in 0..ITERATIONS {
        report("syscall", "ns/op", measure_syscall());
    }
    for _ in 0..ITERATIONS {
        report("mmap", "ns/MiB", measure_mmap());
    }
    for _ in 0..ITERATIONS {
        report("futex", "ns/roundtrip", measure_futex());
    }
    for _ in 0..ITERATIONS {
        report("threads", "ns/total", measure_threads());
    }

    println!("BENCH DONE");
    std::process::exit(0);
}
