# Architecture

## Scope

MadRust targets a Linux-ABI-compatible x86_64 kernel: it loads ELF binaries,
implements the Linux system call ABI, and runs unmodified distribution
userland. It is not a general-purpose distribution, it does not replace Linux,
and it is not production ready. Its purpose is to produce a kernel that other
people can read, learn from, and measure.

## Current state

Milestones M0 to M3 are implemented, verified locally and asserted in CI:

- M0: the kernel boots under QEMU through the Xen PVH handover, reports the
  physical memory map and the CPU vendor over the serial line, and exits with
  a deterministic status.
- M1: exceptions are delivered with the full register context and are
  recoverable, the 8259 PIC is remapped, the PIT drives a 100 Hz tick, the
  double fault has its own stack, and the boot ends after 100 ticks.
- M2a: a buddy allocator over 4 KiB physical frames and a size class kernel
  heap on top of it, with the allocator logic unit tested on the host and a
  self test on the target.
- M2b: the kernel runs from the higher half with a direct map of physical
  memory, so a frame index is no longer a virtual address.
- M3: kernel threads with a preemptive round-robin scheduler, interrupt safe
  locks and a recursion guard.

The next milestone is M4: the Linux system call ABI, the ELF loader and a
static userland. Before the benchmark phase, the frame metadata has to become
dynamic so the kernel can use more than 1 GiB of RAM (see the limits below).

## Repository layout

A cargo workspace with one crate per concern:

| Crate | Contents |
|---|---|
| `madrust-arch` | x86_64 hardware access: port I/O, control registers, MSRs, the boot trampoline. The only crate allowed to be architecture-specific. |
| `madrust-bootinfo` | Pure parsing of the boot handover structures (PVH `start_info`, memory map). Host-testable, no hardware access. |
| `madrust-drivers` | Device drivers: 16550A UART, PIT, PIC, later PCI/IDE/VirtIO. |
| `madrust-kernel` | The `no_std` kernel binary: entry, memory management, scheduler, VFS, syscall layer. |
| `madrust-abi` | System call numbers, `errno` values, user-visible structure layouts. Shared by kernel and test userland. |
| `madrust-elf` | ELF parser. Pure, host-testable. |
| `madrust-mm` | Physical frame allocator (buddy), kernel heap (slab), virtual memory. |
| `madrust-sched` | Kernel threads, wait queues, scheduler. |
| `madrust-vfs` | VFS interfaces, ramfs/tmpfs, cpio initramfs. |
| `madrust-userland` | Static test binaries used as kernel test fixtures. |

Crates are added when the milestone that needs them arrives, not before. A
crate with one function does not exist yet.

## Design decisions

- **`no_std`, panic = abort.** The kernel is the runtime; `std` assumes an OS
  underneath. No unwinding, no `std` collections: `alloc` plus a small set of
  intrusive structures written for this kernel.
- **Uniprocessor first, SMP at M9.** Concurrency bugs are much cheaper to
  diagnose on one CPU. SMP is the last milestone, not the first.
- **Interrupt controller: 8259 PIC + PIT until M8**, then LAPIC/IOAPIC behind a
  small trait, because EOI and masking semantics differ.
- **Paging: 4 levels, kernel in the higher half** (`0xffff8000_00000000`),
  physical memory direct-mapped with 2 MiB pages, user space below.
- **System calls: `SYSCALL`/`SYSRET` only.** `INT 0x80` is not implemented.
- **No async.** Kernel threads, wait queues and spinlocks with interrupts
  disabled locally. Sleeping while holding a lock is forbidden.
- **Threads (M3):** one CPU, one address space: a thread differs only in its
  kernel stack, so a switch is a register exchange plus a stack pointer
  change. The run queue is a fixed size array of eight slots, so the
  scheduling path never allocates. Each thread owns a page aligned 16 KiB
  stack from the kernel heap.
- **Preemption (M3):** the timer handler calls the scheduler every tick, ten
  milliseconds at 100 Hz. The handler runs on the stack of the thread it
  interrupted, above that thread's interrupt frame, and the switch saves that
  stack pointer as the thread context. The interrupted frame is never copied:
  it stays exactly where the CPU and the stubs put it, and the thread resumes
  by returning through `iretq`. No interrupt is lost and no thread ever
  resumes in the middle of an instruction.
- **Locking policy (M3):** the kernel spinlock saves RFLAGS, clears the
  interrupt flag, acquires, and restores RFLAGS on release. Holding the lock
  therefore also blocks preemption, which is what makes the lock safe on a
  single CPU: the holder always finishes its critical section, and a thread
  that spins on the lock cannot be preempted while it waits. Two rules follow
  and must be respected: never hold a lock across a context switch, and never
  block while holding one. A recursion guard reports a thread that takes a
  lock it already holds instead of deadlocking on it.
- **Allocator:** buddy allocator for physical frames, slab allocator with size
  classes for kernel objects. Not rtlmalloc: its semantics are a poor fit for
  kernel use and its interrupt-safety guarantees are weaker.
- **Memory management (M2a):** two layers. A buddy allocator over 4 KiB
  frames, with one metadata byte per frame recording whether the frame is the
  head of a free block, the head of an allocated block, covered by a larger
  block, or never released. The stored order of an allocated block is what
  makes a double free and a free with the wrong order detectable instead of
  silently corrupting the free lists. On top of it, a size class heap with
  the classes 16, 32, 64, 128, 256, 512 and 1024 bytes, each class owning up
  to four one-frame slabs that go back to the frame allocator when the last
  live block is freed. Requests above 1024 bytes take `2^n` whole frames.
- **Reserved memory:** the first MiB (real mode structures, firmware data,
  null page), the kernel image itself, the boot page tables and the boot
  stack are reserved before any frame is released. Frames that are not backed
  by RAM, including the gap between the low RAM region and the region that
  starts at 1 MiB, are never released and therefore never handed out.
- **Allocator bookkeeping never lives in the heap it manages.** The frame
  metadata and the heap free lists are static buffers; the heap cannot
  allocate its own bookkeeping.
- **The allocator logic is host tested.** The buddy algorithm is generic over
  a metadata store trait, with a static implementation in the kernel and a
  `Vec` backed one in the unit tests, so splitting, merging, exhaustion and
  double free detection are covered by `cargo test` and not only by the
  on-target self test.
- **Address space (M2b):** the kernel runs from `0xffffffff80000000` and the
  physical memory is mapped at `0xffff888000000000` with 2 MiB pages. A frame
  index is not an address: the heap translates through the direct map, and
  the reverse translation rejects a pointer outside the direct map instead of
  turning it into a bogus frame.
- **Current limits.** The frame metadata is a static buffer of one byte per
  frame, so the kernel manages at most 1 GiB of physical memory
  (`MAX_FRAMES = 262144` frames). The direct map reserves four page
  directories, so it covers 4 GiB. A guest with more RAM than the metadata
  can describe is refused at boot with a clear message and exit status 37,
  never a panic or a hang. Both limits have to go before the benchmark phase,
  where the rig uses 4 GiB guests.
- **Panic:** print the faulting RIP, the saved registers and the fault address,
  then halt. Do not attempt to kill a single thread.
- **Exceptions are fatal by default.** The only recoverable exception is the
  divide-by-zero injected by the fault injection test, and only when the
  faulting RIP matches the registered trigger instruction. Every other
  exception prints the vector, a readable name and the RIP, and then stops
  the kernel: returning to the faulting instruction would repeat the same
  fault forever. A page fault additionally reports CR2.
- **Interrupts:** the 8259 PIC is remapped to vectors 0x20-0x2F so that
  hardware interrupts never collide with CPU exceptions, and only the timer
  line is unmasked. The PIT channel 0 generates a 100 Hz tick. The double
  fault runs on its own stack through IST1, so a broken kernel stack does not
  turn a fault into an undiagnosed triple fault.
- **Interrupt stubs are generated.** `scripts/gen-isrs.sh` regenerates
  `madrust-kernel/src/isrs.s` from the table of vectors that push an error
  code. The file is never edited by hand.
- **Testability:** hardware sits behind traits (`Serial`, `Clock`, `Interrupt`);
  pure logic lives in crates that run on the host under `cargo test`. The
  on-target test suite runs inside QEMU and asserts over the serial log.
- **No KPTI, no retpoline, no SMAP enforcement.** MadRust runs in a private
  single-tenant VM. This is a deliberate benchmark advantage and a deliberate
  security limitation; both are documented, never quietly.

## Milestone ladder

Each milestone has an exit criterion that a machine can check.

| M | Exit criterion | Verification |
|---|---|---|
| M0 | QEMU serial log contains `MadRust: boot OK`, QEMU exits 33 | headless QEMU + serial assertion |
| M1 | Divide by zero prints `DIV0 RIP=...` and continues; 100 PIT ticks print `TICKS 100` | golden serial log |
| M2 | 10,000 allocate/free cycles on target without leaks or double frees; usable RAM matches `-m` | host unit tests + on-target self test |
| M3 | 10 threads make progress under preemption with fair scheduling | on-target self test |
| M4 | A statically linked `hello` from an initramfs prints and exits 0; ~20 syscalls return the same values as Linux | golden log + diff against mainline |
| M5 | `/init` busybox shell runs `ls | wc -l` with the same output as Linux | diff suite |
| M6 | A musl dynamic binary runs from a read-only ext2 rootfs | golden log |
| M7 | Signal and pipeline test suite produces identical output to Linux | diff suite in CI |
| M8 | Published benchmark report: raw CSV, median, p99, sigma, N >= 30 | `bench/` scripts |
| M9 | Boots with 4 vCPUs; parallel workload scales at least 2.5x over 1 vCPU | litmus tests + benchmark |

Estimated effort for a solo developer with steady spare time: about 34 weeks
total, roughly 12 hours per week.

## Risks

1. Scope creep towards "a small Linux". The cut list is frozen: no network
   stack, no kernel modules, no namespaces, no cgroups.
2. Dynamic loader (TLS, relocations) is the hardest single item. Mitigation:
   static musl binaries first (M4), dynamic musl later (M6).
3. ABI divergence in `errno` values or structure layouts. Mitigation: diff the
   same binary's behaviour on Linux and MadRust from M4 onwards.
4. Concurrency bugs once SMP arrives. Mitigation: lockdep-lite from M3, one CPU
   until M8.
5. Burnout. Mitigation: visible weekly exit criteria, stop rules below.
6. QEMU is not hardware. Bare metal is an explicit non-goal; all results are
   labelled as QEMU results.

## Stop rules

When the schedule slips, cut in this order: network stack (not started), SMP
(M9, publish single-CPU benchmarks instead), dynamic linking (M6, static musl
only), signals (M7, keep kill/exit/SIGCHLD only), benchmark suite (M8, keep
only the getpid and context-switch benchmarks).

M0 to M5 are not cut.

## Contribution to the ecosystem

Mainline Linux does not merge code from an external kernel. The realistic
paths are:

- Publish the reusable pure crates (`madrust-elf`, `madrust-bootinfo`, the
  allocator) as standalone `no_std` libraries usable by other kernel projects.
- Contribute Rust code and drivers to Rust-for-Linux, which is the upstream
  path for Rust in mainline.
- Publish the benchmark methodology and raw results, including the losses.

Credibility comes from green CI, a documented syscall table versioned against
Linux, small reviewable commits, and code that reads like a person wrote it.
