# MadRust

[![CI](https://github.com/NicolasBecasAzagra/MadRust/actions/workflows/ci.yml/badge.svg)](https://github.com/NicolasBecasAzagra/MadRust/actions/workflows/ci.yml)

A Linux-ABI-compatible operating system kernel written entirely in Rust.

MadRust is a from-scratch x86_64 kernel that speaks the Linux userspace ABI: the
same system calls, the same `struct` layouts, the same ELF loading rules. The
long-term goal is to boot an unmodified Debian/Ubuntu userland on it, and to
benchmark it against mainline Linux under identical, published conditions.

Everything in this repository is written in Rust. The only foreign code is the
bootloader-free PVH boot protocol handover implemented by QEMU itself, and the
SPDX headers below.

## Status

Everything below is implemented, verified locally and asserted in CI. One
commit per milestone.

| Milestone | What it delivers | State |
|---|---|---|
| M0 | Boot through the Xen PVH handover, serial output, physical memory map | done |
| M1 | GDT, TSS with an IST stack, IDT, recoverable exceptions, 8259 PIC, PIT | done |
| M2 | Buddy allocator, size class heap, higher half kernel, direct map | done |
| M2b | The kernel runs from the higher half and reaches physical memory only through the direct map | done |
| M2c | Frame metadata carved out of the memory it describes, 32 GiB of guests | done |
| M3 | Kernel threads, preemptive round robin scheduler, interrupt safe locks | done |
| M4 | Ring 3, `SYSCALL`/`SYSRET`, ELF64 loader, 58 Linux system calls | done |
| M4b | A real userland: a standard library program running unmodified | done |
| M4c | Demand paging for anonymous mappings | done |
| M8 | Benchmark rig, six workloads, the comparison published with its raw data | done |
| M9 | SMP: ACPI MADT, INIT-SIPI-SIPI, LAPIC and IOAPIC, IPIs, four processors | done |
| E1 | The initramfs as a read only file system | done |
| E2 | A file backed mapping and large pages | done |
| E2b | Large pages for file mappings and `mlock` residency of the weights | done |
| E3a | A scheduler with priorities and wakeup preemption | done |
| E3b | The tail of the scheduler measured, and the policy that shortens it | done |
| E4 | The inference runtime: a real model, token for token against the reference | done |
| E5 | The measurement of the runtime on both kernels, same machine, same binary | done |
| E6 | The agent: model, tool call, tool result and model, with a grammar that forces the call | done |

What comes next, in the order it has to happen, is the path to the final
artefact: an operating system that serves models to agents, faster than the
incumbent.

| Next | Why it is next |
|---|---|
| The measurement of the agent on both kernels | The latency of a step of the loop is the number an agent server is bought for |
| Networking | An agent that cannot call an API is a demo; with it, it is a server |
| Quantization | A model that does not fit in the memory of the machine cannot be served |
| A scheduler with a better tail | Batching inference requests lives on the tail of the scheduler |

Deferred, and named so they are not lost: processes and a real file system,
dynamic linking, signals and a shell test suite. The reasoning is in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

Exit criteria for each milestone are objective and machine-checkable. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## What this project is not

Being "better than Linux at everything" is not a goal, and it is not a
measurable one. MadRust publishes a narrow, defensible claim instead:

- Same userspace, same QEMU version, same machine model, same rootfs, same
  benchmark scripts, raw results committed to the repository.
- Two areas where a purpose-built kernel can plausibly win in a private VM:
  syscall entry cost (no KPTI/retpoline in a trusted single-tenant guest) and
  fork/exec plus tail latency (no LSM, no audit, no cgroups, no hooks).
- Every loss is published too. No cherry-picking, no "mitigations off!" as the
  only comparison point.

The methodology is specified in [docs/BENCHMARKING.md](docs/BENCHMARKING.md).

### First measurement

MadRust against Linux 7.3.0, one processor, 512 MiB, thirty iterations per
workload, under emulation:

| Workload | MadRust | mainline | mainline, mitigations off |
|---|---|---|---|
| system call | 245.5 ns | 9581 ns | 520 ns |
| map and touch memory | 1.768 ms/MiB | 7.792 ms/MiB | 3.263 ms/MiB |
| fault in memory | 907.6 us/MiB | 12.700 ms/MiB | 4.117 ms/MiB |
| slowest page fault | 474.8 us | 1293.4 us | 572.9 us |
| futex round trip | 3.56 us | 128.5 us | 24.06 us |
| threads | 224.7 us | 1566.5 us | 823.5 us |

With the mitigations disabled on both sides, MadRust is about twice as cheap
per system call, four and a half times as cheap per MiB faulted in and seven
times as cheap per futex round trip, because it does much less work per
operation. An earlier run is published too, with twenty seven missing samples
from a boot that did not finish, because the rig records what happened
instead of hiding it.

Every number above was produced by an emulator, so the ratios are inflated
by the translation overhead both kernels pay, and the two tail measurements
have five samples each. A run with hardware acceleration is required before
any statement about wall clock performance. The full reading, the caveats and
the raw iterations are in [docs/BENCHMARKING.md](docs/BENCHMARKING.md) and in
`bench/results/`.

## Build and run

Requirements:

- Rust nightly (pinned in `rust-toolchain.toml`), with the `rust-src` and
  `llvm-tools` components.
- QEMU 11.x (`qemu-system-x86_64`).
- `xorriso` and `mtools` only if you build disk images.

```sh
rustup toolchain install nightly-2026-10-24 \
    --component rust-src --component llvm-tools --component rustfmt --component clippy
make build          # cargo build --release
make run            # boot in QEMU, serial on stdout
make test           # host unit tests
make check          # fmt + clippy
```

Boot directly with QEMU:

```sh
qemu-system-x86_64 -M q35 -m 512M \
    -kernel target/x86_64-madrust/release/madrust-kernel \
    -display none -monitor none -no-reboot -serial stdio \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
```

Debugging: add `-s -S` to QEMU (gdbstub on port 1234, CPU halted), then attach
`lldb target/x86_64-madrust/release/madrust-kernel` with `gdb-remote 1234`
(macOS) or `gdb` with `target remote :1234` (Linux).

The full Linux VM setup (for building mainline Linux and distro rootfs) is in
[docs/VM-SETUP.md](docs/VM-SETUP.md).

## Repository layout

```text
madrust-arch/       x86_64 hardware access, port I/O, boot trampoline
madrust-bootinfo/   pure parsing of the PVH boot handover structures
madrust-drivers/    16550A UART, isa-debug-exit
madrust-mm/         paging, the buddy allocator, the heap, the mappings
madrust-sched/      the scheduler and its policy
madrust-elf/        the ELF64 reader
madrust-initramfs/  the reader of the cpio archive of the initramfs
madrust-kernel/     the no_std kernel binary
madrust-userland/   the test program that runs unmodified in ring 3
madrust-bench/      the workload that measures the kernel
madrust-infer/      the inference runtime, the grammar and the chat prompt
madrust-agent/      the agent loop: model, tool call, tool result
bench/              the rig, the workloads, the reference build, the datasets
docs/               architecture, boot protocol, benchmarking, VM setup
scripts/            ISO and CI helper scripts
```

Dependencies policy: kernel crates may only use MIT, BSD or ISC licensed code.
`GPL-2.0-only` is incompatible with `Apache-2.0-only`, so Apache-only crates
are forbidden in the kernel.

## Boot protocol

MadRust boots through the Xen PVH handover (`XEN_ELFNOTE_PHYS32_ENTRY`), which
QEMU supports natively for 64-bit ELF kernels. Multiboot2 is not an option:
QEMU's Multiboot loader rejects 64-bit images. The same path also boots
mainline Linux (`vmlinux` with a PVH note), which is what makes the benchmark
comparison fair. Details in [docs/BOOT.md](docs/BOOT.md).

## How this was built

MadRust was written with the assistance of an AI agent, and this is stated
here on purpose rather than left for someone to discover.

- The design decisions, the milestone criteria and the verification belong to
  a human. Every milestone has an objective exit criterion that a machine
  checks: a serial log assertion in CI, a unit test on the host, a row in the
  benchmark data. Not "it compiles".
- The code was written by an AI agent against those criteria, and every
  change was checked against the build, the tests and the boot log before it
  was committed. Several of the defects this repository fixed were found by
  that checking, not by the code looking wrong.
- The repository is the evidence: one milestone per commit, host tests for
  everything that can be tested on the host, and a benchmark rig that
  publishes its raw data with the failures included.
- What an agent cannot do is explain why a line is there. The reading path in
  [docs/LEARNING.md](docs/LEARNING.md), with its experiments, is the human
  part of the work.

The method is the reusable part. "How much of an operating system can be
written this way, and how do you know it works" is a question worth an
answer, and this repository is one answer to it.

## Documentation

- **[REPORT.md](REPORT.md)** - the measurements, the findings, the losses and the threats to validity.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) - crates, design decisions, milestone ladder, risks, stop rules.
- [docs/BOOT.md](docs/BOOT.md) - PVH boot protocol, trampoline, memory map, debugging.
- [docs/BENCHMARKING.md](docs/BENCHMARKING.md) - the method, and the raw numbers with their caveats.
- [docs/LEARNING.md](docs/LEARNING.md) - how to read the kernel, experiments to run, sources to read next.
- [docs/VM-SETUP.md](docs/VM-SETUP.md) - the Linux build host: mainline Linux, rootfs, and the artifact handoff.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Code, comments and commits are in
English. Every file carries an SPDX identifier, every `unsafe` block carries a
`SAFETY:` justification, and CI must be green.

## License

`GPL-2.0-only`. See [LICENSE](LICENSE).
