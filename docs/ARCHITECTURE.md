# Architecture

## Scope

MadRust targets a Linux-ABI-compatible x86_64 kernel: it loads ELF binaries,
implements the Linux system call ABI, and runs unmodified distribution
userland. It is not a general-purpose distribution, it does not replace Linux,
and it is not production ready. Its purpose is to produce a kernel that other
people can read, learn from, and measure.

## The userland and the system call table (M4b)

The test userland is a normal Rust program built against the standard
library, statically linked for `x86_64-unknown-linux-musl` with the bundled
`rust-lld`, so no C compiler is involved anywhere. It is a position
independent executable of about 560 KB, loaded at `0x400000` and entered at the
base plus `e_entry`.

The system call table uses the Linux numbering, the Linux argument order and
the Linux error numbers. A call that is not implemented returns `-ENOSYS` and
the kernel prints its number, so the gaps are visible in the boot log instead
of hiding.

Implemented:

```text
read 0        write 1       close 3       fstat 5       poll 7
mmap 9        mprotect 10   munmap 11     brk 12        rt_sigaction 13
rt_sigprocmask 14           ioctl 16      sched_yield 24 mremap 25
madvise 28    dup 32        dup2 33       nanosleep 35  getpid 39
exit 60       kill 62       uname 63      fcntl 72      getcwd 79
readlink 89   gettimeofday 96             getrusage 98  getuid 102
getgid 104    geteuid 107   getegid 108   getppid 110   sigaltstack 131
prctl 157     arch_prctl 158               gettid 186    futex 202
sched_getaffinity 204                      getdents64 217
set_tid_address 218                       clock_gettime 228
clock_nanosleep 230                       exit_group 231 tgkill 234
openat 257    newfstatat 262              set_robust_list 273
dup3 292      pipe2 293     prlimit64 302 getrandom 318 membarrier 324
statx 332     rseq 334
```

Not implemented yet, and they return `-ENOSYS`: `lseek` 8, `clone` 56,
`execve` 59, `wait4` 61, and everything that needs sockets or a file system.

### What the implementations do not do yet

These are deliberate limits of the milestone, not accidents, and each one is a
task for M5 or M6:

- `mmap` serves anonymous memory only. A file backed mapping returns
  `ENODEV`, because there is no file system yet.
- `mprotect` toggles write permission only. The `NXE` bit is not enabled and
  `PROT_NONE` is not enforced.
- Pipes are 4 KiB buffers that return `EAGAIN` instead of blocking, and
  `poll` never blocks.
- `openat`, `newfstatat`, `statx`, `getdents64` and `readlink` answer
  `ENOENT`, `ENOTDIR` or `EINVAL`: there is no file system.
- `FUTEX_WAIT_BITSET` timeouts are treated as relative.
- The futex wait table holds eight waiters.

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
- M2c: the frame metadata is carved out of the memory it describes, so a
  guest is no longer limited to 1 GiB. The direct map describes 32 GiB.
- M3: kernel threads with a preemptive round-robin scheduler, interrupt safe
  locks and a recursion guard.
- M4a: user mode. Ring 3 descriptors, `SYSCALL`/`SYSRET`, an ELF64 loader, and
  a user program that runs, prints through the `write` system call and exits.
- M4b: a real userland. A standard library program, statically linked for the
  Linux musl target, runs unmodified: the loader learned position independent
  executables and the system call table grew to what `std` needs.

The next milestone is M5: a file system, an initramfs and process support, so
the kernel can run a distribution init instead of a single program. Before the
benchmark phase, the frame metadata has to become dynamic so the kernel can
use more than 1 GiB of RAM (see the limits below).

## The endgame: serving a real model

The project exists to make one measurement that matters for the workloads of
the next decade: what does the kernel cost an inference server.

The final artefact is a real language model, served by a real inference
runtime written in Rust, measured against the same runtime and the same model
on mainline Linux, in the same virtual machine, with the same flags. The
numbers are tokens per second and the time to the first token.

The plan, in the order it has to happen:

1. **Dynamic frame metadata.** A model is hundreds of megabytes; a guest that
   cannot describe more than 1 GiB of memory cannot hold one.
2. **A read only file system over the initramfs.** The archive is already
   parsed, so exposing its files through `openat`, `read` and `lseek` is what
   turns the initramfs into something a model file can be loaded from.
3. **Large pages.** Weights are faulted in page by page today. Mapping them
   with 2 MiB pages cuts the number of faults by a factor of 512, and it is
   the single change that matters most for loading a model.
4. **A scheduler with a better tail.** The threads workload is the one
   measurement where MadRust loses. Inference servers batch requests, and
   batching lives or dies on the latency of the tail, not on the average.
5. **The runtime.** A forward pass in Rust: the tokenizer, the weights in
   half precision, the layers, the sampler. The prompt is encoded ahead of
   time, so the runtime only has to decode.
6. **The measurement.** Tokens per second, time to the first token, and the
   cost of the page faults that stream the weights, for both kernels.

Two things this plan deliberately does not do:

- It does not replace Linux for AI training. Nobody is going to run a
  training cluster on this kernel, and pretending otherwise would be a lie
  with a benchmark attached.
- It does not claim a general performance advantage. The claim is narrow and
  measurable: for a single tenant inference server, a kernel that does less
  work per operation and faults weights in with large pages costs less per
  token, and the numbers are published with the losses.

The model for the first run is a small one, so that the whole pipeline fits
in a guest that the kernel can describe: a character or byte level story
model of a few tens of megabytes, with weights in half precision. Once the
pipeline is proven, a larger model is a change of size, not of architecture.

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
| `madrust-initramfs` | Parser for uncompressed `cpio` archives in the `newc` format. Pure, host-testable, no `unsafe`. |
| `madrust-bench` | The benchmark workload binary: a static Linux program that runs on both kernels. |

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
- **User mode (M4a):** the kernel half and the user half live in the same
  page tables for now, so there is one address space and no `CR3` switch.
  Per process address spaces come later. The selectors are `0x08` kernel
  code, `0x10` kernel data, `0x1B` and `0x23` the ring 3 code and data used
  by `iretq`, `0x2B` the ring 3 code used by `sysretq` and `0x30` the TSS.
  The compile time assertions in the GDT module pin that relation, because
  `SYSRET` derives the user selectors from `STAR` and a mismatch is a silent
  fault.
- **Clock:** the guest clock is the time stamp counter, calibrated against
  the timer interrupt. The timer alone ticks at 100 Hz, which would quantise
  every measurement to 10 ms.
- **Initramfs (M8a):** the kernel finds the module QEMU passes with `-initrd`
  through the PVH module list, and parses it as an uncompressed `cpio` archive
  in the `newc` format. The parser lives in its own crate, forbids `unsafe`
  and is unit tested on the host. The program to run is `/bench`, or the
  first executable in the archive. A mainline kernel accepts the same archive,
  which is what makes the benchmark fair.
- **User threads (M8a):** `clone` with the `CLONE_VM`, `CLONE_FS`,
  `CLONE_FILES`, `CLONE_SIGHAND`, `CLONE_THREAD` and `CLONE_SYSVSEM` flags
  creates a thread that shares the address space, with its own kernel stack,
  its own user stack and its own thread local storage base. The base is kept
  in the thread control block and written to the `FS_BASE` register on every
  switch. Flags that are not supported are rejected with `EINVAL`, never
  ignored.
- **System call policy:** the Linux numbering, the Linux argument order
  (number in `RAX`, arguments in `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9`) and
  negative errno values on failure. A call that is not implemented returns
  `-ENOSYS`, never a wrong success. The implemented set is listed in the
  system call table below.
- **System call entry:** the stub swaps to the kernel `GS` base, saves the
  user stack pointer in the per CPU slot, loads the kernel stack from the TSS
  `rsp0`, saves every general purpose register and the user `RIP` and flags,
  and calls the dispatcher with the arguments in the System V order. The
  `SFMASK` masks the interrupt flag, so a system call is never interrupted by
  the timer while it runs. The return path restores the registers, reloads
  the user stack pointer and returns with `sysretq`.
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
- **Demand paging (M4c):** an anonymous mapping reserves address space and
  nothing else. The page table entries are written as not present, and the
  frames arrive when the program touches them: the page fault handler looks
  the address up in a table of user mappings, takes a zeroed frame from the
  buddy allocator, maps it with the protection the record carries and
  returns, so the faulting instruction is retried and succeeds. A table of
  32 slots holds the mappings.
  - A protection violation never replaces a present entry. The handler
    distinguishes it from a missing page and reports it.
  - An address that belongs to no mapping, a write to a mapping that is not
    writable and an out of memory condition stop the program with a clear
    message and exit status 41, never a hang and never a panic.
  - `munmap` removes the record and gives back the frames that were actually
    faulted in, by walking the page tables of the range.
  - `mremap` is not supported and returns `EINVAL`. The program break keeps
    its eager behaviour.
- **Current limits.** The frame metadata is one byte per frame, and it is
  carved out of the physical memory it describes: the kernel takes it from the
  top of the highest RAM region that can hold it, clear of the low megabyte,
  the kernel image and the initramfs, and reserves it before any frame is
  released. The direct map reserves 32 page directories, so it describes
  32 GiB, and the largest buddy order is 20, that is 4 GiB in one block. A
  guest with more memory than the direct map can describe is refused at boot
  with a clear message and exit status 37, never a partial mapping and never
  a panic. The metadata needs 8 MiB for the largest supported guest.
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

Each milestone has an exit criterion that a machine can check. The ones marked
done are implemented and asserted in CI.

| M | Exit criterion | Verification | State |
|---|---|---|---|
| M0 | QEMU serial log contains `MadRust: boot OK`, QEMU exits 33 | headless QEMU + serial assertion | done |
| M1 | Divide by zero prints `DIV0 RIP=...` and continues; 100 PIT ticks print `TICKS 100` | golden serial log | done |
| M2 | 10,000 allocate/free cycles on target without leaks or double frees; usable RAM matches `-m` | host unit tests + on-target self test | done |
| M2b | The kernel runs from the higher half and reaches physical memory only through the direct map | the same byte read before and after the switch | done |
| M2c | A guest of 4 GiB boots, manages its memory and runs the workload | QEMU with `-m 4G` | done |
| M3 | 10 threads make progress under preemption with fair scheduling | on-target self test | done |
| M4 | A statically linked `hello` from an initramfs prints and exits 0; the system calls return the same values as Linux | golden log + diff against mainline | done |
| M4b | A standard library program runs unmodified, prints and exits 0 | golden log | done |
| M4c | An anonymous mapping faults its pages in on demand, and the per page tail measures a real fault | the `weights` workload against mainline | done |
| M8 | Published benchmark report: raw CSV, median, p99, sigma, N >= 30 | `bench/` scripts | done |
| E1 | The initramfs is a read only file system: a user program reads a file from it | `openat`, `read`, `lseek`, `fstat` | next |
| E2 | The weights are mapped with 2 MiB pages | the `weights` workload, before and after | next |
| E3 | A scheduler that keeps the tail of a runnable thread inside a budget | the `threads` workload | next |
| E4 | A Rust inference runtime generates the same tokens as the reference implementation | token for token diff against the reference | next |
| E5 | Tokens per second and time to first token, both kernels, same machine | the rig, with the raw data published | next |
| M5 | `/init` busybox shell runs `ls | wc -l` with the same output as Linux | diff suite | deferred |
| M6 | A musl dynamic binary runs from a read-only ext2 rootfs | golden log | deferred |
| M7 | Signal and pipeline test suite produces identical output to Linux | diff suite in CI | deferred |
| M9 | Boots with 4 vCPUs; parallel workload scales at least 2.5x over 1 vCPU | litmus tests + benchmark | deferred |

The deferred milestones are not abandoned, they are sequenced behind the
endgame: a model has to be read from a file system, and the file system that
matters for the final measurement is the one the model comes from.

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
