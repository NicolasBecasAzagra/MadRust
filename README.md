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

| Milestone | Goal | State |
|---|---|---|
| M0 | Boot under QEMU, print to serial | done |
| M1 | GDT/TSS/IDT, PIC, PIT, panic handler | done |
| M2 | Physical memory, paging, direct map, heap | done |
| M2c | Dynamic frame metadata, guests larger than 1 GiB | in progress |
| M3 | Kernel threads, preemptive scheduler | done |
| M4 | Syscalls, ELF loader, static userland | done |
| M5 | VFS, initramfs, pipes, fork/exec/wait | planned |
| M6 | Dynamic linking (musl), read-only ext2 | planned |
| M7 | Signals, full tty, shell test suite | planned |
| M8 | Benchmark harness and public report | done (first results published) |
| M9 | SMP (LAPIC/IOAPIC) | planned |

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
| system call | 231 ns | 9820 ns | 661 ns |
| map and touch memory | 2.24 ms/MiB | 8.00 ms/MiB | 3.44 ms/MiB |
| futex round trip | 3.93 us | 136.4 us | 31.4 us |
| threads | 1.535 ms | 1.640 ms | 1.029 ms |

With the mitigations disabled on both sides, MadRust spends about a third of
what mainline spends on a system call and an eighth on a futex round trip,
because it does much less work per operation. It is **1.5 times slower** on
the threads workload, and that loss is published with the same raw data.

Every number above was produced by an emulator, so the ratios are inflated
by the translation overhead both kernels pay. A run with hardware acceleration
is required before any statement about wall clock performance. The full
reading, the caveats and the raw iterations are in
[docs/BENCHMARKING.md](docs/BENCHMARKING.md) and in `bench/results/`.

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
crates/madrust-arch      x86_64 hardware access, port I/O, boot trampoline
crates/madrust-bootinfo  pure parsing of the PVH boot handover structures
crates/madrust-drivers   16550A UART, isa-debug-exit
crates/madrust-kernel    the no_std kernel binary
docs/                    architecture, boot protocol, benchmarking, VM setup
scripts/                 ISO and CI helper scripts
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

## Documentation

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) - crates, design decisions, milestone ladder, risks, stop rules.
- [docs/BOOT.md](docs/BOOT.md) - PVH boot protocol, trampoline, memory map, debugging.
- [docs/LEARNING.md](docs/LEARNING.md) - how to read the kernel, experiments to run, sources to read next.
- [docs/BENCHMARKING.md](docs/BENCHMARKING.md) - how comparisons against mainline are run and published.
- [docs/VM-SETUP.md](docs/VM-SETUP.md) - the Linux build host: mainline Linux, rootfs, and the artifact handoff.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Code, comments and commits are in
English. Every file carries an SPDX identifier, every `unsafe` block carries a
`SAFETY:` justification, and CI must be green.

## License

`GPL-2.0-only`. See [LICENSE](LICENSE).
