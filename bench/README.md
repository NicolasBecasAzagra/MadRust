# The MadRust benchmark rig

This directory compares MadRust with mainline Linux under identical conditions:
one workload binary, one initramfs, one QEMU configuration, and only the kernel
image differs. It exists to publish reproducible relative numbers, not to claim
that either kernel is faster on real hardware.

## Quick start

Build the kernel and the workload, then run the MadRust side alone:

```sh
make build
make bench
bench/mkinitramfs.sh
bench/run.py --madrust-only
```

`run.py` builds the initramfs itself when `bench/initramfs.cpio` is missing. The
MadRust image defaults to `make -s print-kernel-path`, so the shortest form is:

```sh
bench/run.py --madrust-only
```

When the mainline image exists, pass both kernels:

```sh
bench/run.py --madrust target/.../madrust-kernel --mainline /path/to/bzImage
```

The reference image has to carry the Xen PVH note, or QEMU refuses a 64-bit
ELF kernel. `bench/prepare-mainline.sh` builds one with `CONFIG_PVH=y`.

The methodology requires the mitigation configuration of the reference kernel
to be published next to the default one, so add it as a variant:

```sh
bench/run.py --madrust target/.../madrust-kernel \
             --mainline artifacts/vmlinux \
             --mainline-variant "nomitig=mitigations=off"
```

A default mainline kernel reloads CR3 on every system call to isolate the
page tables. Under emulation that flush invalidates the translator cache and
inflates the cost of a system call by two orders of magnitude, which says
nothing about the kernel. The variant disables the mitigations on the
reference side, which is the fair comparison against a kernel that never
implements them.

Every run writes into `bench/results/` by default (or `--out DIR`):

```text
madrust.csv        one row per sample of the MadRust run
madrust.log        the serial log of the MadRust run
mainline.csv       the same for mainline, when it was run
mainline.log
summary.csv        median, p99 and standard deviation per workload
summary.json       the summary plus the run metadata
```

## QEMU configuration

Both kernels always run with the same flags:

```text
-M q35 -cpu Skylake-Client -smp 1 -m 512M
-kernel <image> -initrd bench/initramfs.cpio
-append "console=ttyS0"
-display none -monitor none -no-reboot
-serial file:<log>
-device isa-debug-exit,iobase=0xf4,iosize=0x04
```

- `-cpu Skylake-Client` is fixed. `-cpu host` changes between machines and
  invalidates the comparison.
- One vCPU and 512 MiB match what the MadRust scheduler and frame allocator
  support today, and keep the comparison honest: the kernel is a uniprocessor,
  so a multi-vCPU mainline run would measure a different machine.
- `-append "console=ttyS0"` is needed by mainline; MadRust ignores the command
  line.
- The initramfs is an uncompressed `cpio` `newc` archive. Mainline accepts it
  directly and MadRust parses the same bytes, so no decompressor is involved.
- `--accel` selects `tcg` (default), `kvm` or `hvf`. Both kernels use the same
  accelerator in a run, and `summary.json` records which one it was.

## The workload

`madrust-bench` is a static Rust binary for `x86_64-unknown-linux-musl`. It is
packed twice into the archive, at `/bench` (the name MadRust prefers) and at
`/init` (the name mainline runs as PID 1). The bytes are identical, so neither
kernel needs a special path. The archive also carries `/dev/console` and
`/dev/null`: mainline opens the console before it execs `/init`, and a kernel
that boots an initramfs does not mount devtmpfs on its own, so those nodes must
come from the archive or the workload would run without descriptors 0 to 2.
`mkinitramfs.sh` creates them as empty files and rewrites their mode and device
numbers in the finished `newc` archive, because creating a device node needs
root on both macOS and Linux.

The binary runs four measurements, prints one `RESULT <workload> <value> <unit>`
line per sample, and ends with `BENCH DONE`:

| Workload | What it measures | Unit |
|---|---|---|
| `syscall` | A tight `getpid` loop: system call entry and return | ns per call |
| `mmap` | `mmap` an anonymous region, touch every page, `munmap` | ns per MiB |
| `futex` | Two threads ping-pong through one futex word | ns per round trip |
| `threads` | Four threads doing fixed arithmetic, spawn to join | ns for the whole run |

Threads are created with a direct `clone` system call, not `std::thread`, so
the binary runs on both kernels: MadRust implements the sharing flags the
workload passes, and mainline implements all of them.

Every workload prints a line even when the kernel refuses a call. A failure
looks like:

```text
RESULT syscall ERR -38
```

The number is the negated Linux error number. The harness stores it in the
`error` column with `status=error`; nothing is discarded or retried. A sample
that never printed because the guest died is recorded as `status=missing`.

The sample counts are fixed in the binary: 30 samples per workload, 20 000
`getpid` calls per syscall sample, four MiB per mmap sample, eight futex round
trips per sample, and 200 000 arithmetic steps per worker thread. They are
large enough to dominate the clock and small enough to finish in a few seconds
under TCG.

## Reading the CSV

`madrust.csv` and `mainline.csv` have one row per sample:

```text
kernel,workload,iteration,value,unit,status,error
madrust,syscall,0,421,ns/op,ok,
madrust,mmap,0,18234,ns/MiB,ok,
```

- `value` is empty for `error` and `missing` rows.
- `iteration` counts from zero inside each workload.
- `status` is `ok`, `error` or `missing`.
- `error` holds the negated Linux error number of an `error` row.

`summary.csv` has one row per kernel and workload with `samples`, `errors`,
`missing`, `median`, `p99` and `stddev`. `p99` is the nearest-rank 99th
percentile and `stddev` is the population standard deviation. `summary.json`
adds the QEMU version, the host, the SHA-256 of both kernel images and of the
initramfs, the exact command line of each run, the accelerator and the start
time.

## What the numbers mean

The comparison is between two kernels running the same binary under the same
emulation. It is not a comparison between a kernel and a distribution, and it
does not predict the performance of either on real hardware.

A host without hardware acceleration runs QEMU in TCG mode. Every guest
instruction, including every system call trap, is translated, so absolute
numbers describe the emulator. A TCG run still supports a relative statement
because both kernels see identical emulation. `summary.json` records the
accelerator, and the report must state it.

Boot time is not measured: the boot paths differ, and boot time is not the
kernel property this rig compares.
