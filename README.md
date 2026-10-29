# MadRust

[![CI](https://github.com/NicolasBecasAzagra/MadRust/actions/workflows/ci.yml/badge.svg)](https://github.com/NicolasBecasAzagra/MadRust/actions/workflows/ci.yml)

**An operating system for local AI agents, written in Rust from the kernel up.**

MadRust is a from-scratch x86_64 kernel that speaks the Linux userspace ABI:
the same system calls, the same `struct` layouts, the same ELF loading rules.
On top of that kernel sit two programs that are not toys: an inference runtime
that serves a real language model with no dependency outside the Rust standard
library, and an agent that asks the model what to do and runs the tools.

The goal is a machine that serves models to agents, faster and with less
memory than the incumbent, and that shows its numbers.

```text
        the agent            the runtime              the kernel
   model, tool, result   weights, forward pass,     paging, scheduling,
   over five tools       bytes, a byte grammar      memory, processes
        |                        |                        |
        +------------------------+------------------------+
                                 |
                     Linux ABI, unmodified programs
```

## What we are building

**The standard way to run local agents on a person's own machine, fast.**

An agent of today expects three things from the machine it runs on: it calls
APIs over HTTP, it reads and writes files, and it runs commands. It expects a
model that fits, and it expects the answers to come back in milliseconds. A
product that only does the third of that is a demonstration.

MadRust is that machine, built in three layers, each one measurable against
mainline Linux on the same machine, with the same binary and the same
checkpoint.

1. **A kernel that does less.** No page table isolation, no LSM, no audit, no
   cgroups, no hooks. Demand paging, large pages, a scheduler with a short
   tail, and real multiprocessing. Every number is published, including the
   losses.
2. **A runtime that serves a model.** The weights are mapped from the
   initramfs without being copied, kept resident, and read by a decoder
   written from scratch: RMSNorm, rotary embeddings, grouped query attention,
   the gated feed forward block and a key value cache. It generates the same
   tokens as the reference implementation, one for one.
3. **An agent that acts.** The model answers with a tool call, the kernel
   runs the tool, and the model answers again. The shape of the call is not a
   request in the prompt but an automaton over bytes: the answer cannot be
   anything but a well formed call of a declared tool, with the keys that tool
   needs, each of them once, with the type the tool expects.

## What it does today

| | MadRust | mainline | mainline, mitigations off |
|---|---|---|---|
| system call | **376 ns** | 29.3 us | 1.04 us |
| map and touch memory | **6.83 ms/MiB** | 25.8 ms/MiB | 8.69 ms/MiB |
| fault in memory, 4 KiB pages | **2.01 ms/MiB** | 44.6 ms/MiB | 13.7 ms/MiB |
| fault in memory, 2 MiB pages | **825 us/MiB** | 44.9 ms/MiB | 13.0 ms/MiB |
| futex round trip | **6.53 us** | 407 us | 97.7 us |
| four threads, spawn to join | **620 us** | 7.73 ms | 4.59 ms |
| boot to the first result | **12.9 s** | 206 s | 63.8 s |
| resident memory of the guest | **319 MB** | 476 MB | 483 MB |
| time to the first token | **11.25 s** | 24.67 s | 22.82 s |
| tokens per second | **0.45** | 0.20 | 0.21 |
| seconds of an agent loop | **821** | 2793 | 2570 |
| resident memory with the model | **857 MB** | 1021 MB | 1023 MB |

One processor, 512 MiB for the operations and 1024 MiB for the model and the
agent, QEMU 8.2.2, Linux 7.3.0 built from the mainline tree by
`bench/prepare-mainline.sh`, the same initramfs and the same binary on both
sides. Every number of the table comes from a committed dataset:

| rows | dataset |
|---|---|
| the operations | `bench/results/vps-1cpu/` |
| the model | `bench/results/inference-native/` |
| the agent | `bench/results/agent/` |
| the network | `bench/results/net/` |

The network is the fourth layer: the guest enumerates the bus, claims a
network device, resolves the address of the gateway, sends a datagram to a
server on the host and reads the answer, with the socket calls of the Linux
ABI. The agent has a tool that does it, so the loop of the agent reaches
the network.

Three things about these numbers:

- **The ratios are the claim.** Between 1.3 and 7.4 times cheaper per
  operation, three times faster in the agent loop, two thirds of the memory of
  the guest.
- **The absolute numbers describe an emulator.** There is no hardware
  acceleration on the host, so a wall clock statement about a processor is not
  supported yet. A host with KVM is the open issue for that.
- **Every loss is published.** The reference kernel without mitigations is
  measured too, and the batches where a boot did not finish are in
  `bench/results/` with the samples that are missing.

## Status

Everything below is implemented, verified on the host and asserted in the
continuous integration. One commit per milestone.

| Milestone | What it delivers | State |
|---|---|---|
| M0 | Boot through the Xen PVH handover, serial output, physical memory map | done |
| M1 | GDT, TSS with an IST stack, IDT, recoverable exceptions, 8259 PIC, PIT | done |
| M2 | Buddy allocator, size class heap, higher half kernel, direct map | done |
| M2b | The kernel reaches physical memory only through the direct map | done |
| M2c | Frame metadata carved out of the memory it describes, 32 GiB of guests | done |
| M3 | Kernel threads, preemptive round robin scheduler, interrupt safe locks | done |
| M4 | Ring 3, `SYSCALL`/`SYSRET`, ELF64 loader, 59 Linux system calls | done |
| M4b | A real userland: a standard library program running unmodified | done |
| M4c | Demand paging for anonymous mappings | done |
| M8 | Benchmark rig, seventeen measurements, the comparison published with its raw data | done |
| M9 | SMP: ACPI MADT, INIT-SIPI-SIPI, LAPIC and IOAPIC, IPIs, four processors | done |
| M10 | The network: the bus PCI, a virtio device, IPv4 and UDP, the sockets of the ABI | done |
| E1 | The initramfs as a read only file system | done |
| E2 | A file backed mapping and large pages | done |
| E2b | Large pages for file mappings and `mlock` residency of the weights | done |
| E3a | A scheduler with priorities and wakeup preemption | done |
| E3b | The tail of the scheduler measured, and the policy that shortens it | done |
| E4 | The inference runtime: a real model, token for token against the reference | done |
| E5 | The measurement of the runtime on both kernels, same machine, same binary | done |
| E6 | The agent: model, tool call, tool result and model, with a grammar that forces the call | done |

## What comes next

In the order it has to happen. The reasoning and the exit criteria of each one
are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), and the issues of the
tracker carry the same criteria.

| Next | Why it is next |
|---|---|
| **TCP and HTTP** | An agent that cannot call an API over HTTP cannot be used. The protocols of the path are already pure and tested; TCP is the piece that is missing. |
| **The tools an agent of today expects** | Files of the machine and commands: the agent of today reads the files of the person and runs things. |
| **The model context protocol** | The way the clients of today talk to a model and to its tools. An agent that speaks it works with the clients people already have. |
| **A checkpoint that closes the conversation** | The model of 135 million parameters reads a file and then asks for it again. The grammar is not the problem: the checkpoint is. |
| **A host with hardware acceleration** | The only thing missing for the timings to describe a machine instead of an emulator. |
| **A scheduler with a better tail** | Batching inference requests lives on the tail of the scheduler. |

Deferred, and named so they are not lost: processes and a real file system,
dynamic linking, signals and a shell test suite.

## Build and run

Requirements:

- Rust nightly (pinned in `rust-toolchain.toml`), with the `rust-src` and
  `llvm-tools` components.
- QEMU 8.2 or newer (`qemu-system-x86_64`).
- `xorriso` and `mtools` only if you build disk images.

```sh
rustup toolchain install nightly-2026-10-24 \
    --component rust-src --component llvm-tools --component rustfmt --component clippy
make build          # the kernel
make run            # boot it in QEMU, serial on stdout
make test           # host unit tests of every crate
make fmt-check      # the format of every crate
make clippy         # the clippy of every crate, for its target
make clippy-host    # the clippy of every crate, for the host
```

Boot directly with QEMU:

```sh
qemu-system-x86_64 -M q35 -m 512M \
    -kernel "$(make -s print-kernel-path)" \
    -display none -monitor none -no-reboot -serial stdio \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
```

Debugging: add `-s -S` to QEMU (gdbstub on port 1234, CPU halted), then attach
`lldb "$(make -s print-kernel-path)"` with `gdb-remote 1234`
(macOS) or `gdb` with `target remote :1234` (Linux).

### The comparison against mainline

```sh
bench/prepare-mainline.sh ~/madrust/artifacts   # the reference image, on a Linux host
bench/fetch-model.sh ~/madrust/artifacts/model-instruct   # the checkpoint, verified
python3 bench/run.py \
    --madrust "$(make -s print-kernel-path)" \
    --mainline ~/madrust/artifacts/vmlinux \
    --mainline-variant "nomitig=mitigations=off" \
    --out bench/results
```

### The agent

```sh
make agent agent-initramfs
EXTRA_DIR=~/madrust/artifacts/model-instruct \
    bench/mkinitramfs.sh ~/madrust/artifacts/agent.cpio madrust-agent/madrust-agent.elf
python3 bench/run.py --madrust "$(make -s print-kernel-path)" \
    --mainline ~/madrust/artifacts/vmlinux \
    --initrd ~/madrust/artifacts/agent.cpio --memory 1024M \
    --only agent-steps,agent-tokens,agent-seconds,agent-step-ms
```

The full Linux VM setup (for building mainline Linux and a distro rootfs) is in
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
madrust-infer/      the inference runtime, the grammar, the merge encoder
madrust-agent/      the agent loop: model, tool call, tool result
bench/              the rig, the workloads, the reference build, the datasets
docs/               architecture, boot protocol, benchmarking, VM setup
scripts/            the generator of the interrupt stubs
```

Dependencies policy: kernel crates may only use MIT, BSD or ISC licensed code.
`GPL-2.0-only` is incompatible with `Apache-2.0-only`, so Apache-only crates
are forbidden in the kernel.

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
- [docs/LEARNING.md](docs/LEARNING.md) - how to read the kernel and the userland, experiments to run, sources to read next.
- [docs/VM-SETUP.md](docs/VM-SETUP.md) - the Linux build host: mainline Linux, rootfs, and the artifact handoff.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Code, comments and commits are in
English. Every file carries an SPDX identifier, every `unsafe` block carries a
`SAFETY:` justification, and CI must be green.

## License

`GPL-2.0-only`. See [LICENSE](LICENSE).
