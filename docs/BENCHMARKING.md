# Benchmarking

The point of MadRust is not to claim that it is faster than Linux. It is to
publish comparisons that are reproducible and fair, and to let the numbers
decide.

## Rules

1. **Same host, same QEMU, same machine model.** Pin the QEMU version. Use a
   fixed CPU model such as `-cpu Skylake-Client`. Never `-cpu host`: it
   changes between machines and invalidates results.
2. **Same machine shape.** Identical vCPU count, RAM and device set for both
   kernels.
3. **Same userland.** The same Debian or Ubuntu rootfs, the same binaries, the
   same command lines.
4. **Same boot path.** Both kernels are booted with the same QEMU invocation:
   the same `-initrd`, the same `-append`, the same memory size. Only the
   kernel image differs.
5. **Measure the workload, never the boot.** The boot paths differ; boot time
   is not a kernel property worth comparing.
6. **Mitigation configurations are always published.** Mainline Linux is
   measured twice: with its default configuration, and with
   `mitigations=off`. Reporting only the default, or only the mitigated-off
   run, is cherry-picking.
7. **N >= 30 iterations.** Report median, p99 and standard deviation. Commit
   the raw per-iteration data as CSV.
8. **Every loss is published too.** The report includes the benchmarks where
   mainline wins, with the same rigour.
9. **QEMU results are labelled as QEMU results.** No claim about physical
   hardware.

## Measurement rig

- Dedicated host, no other VMs running, CPU frequency governor set to
  `performance`, the benchmark process pinned to a dedicated core with
  `taskset`.
- KVM or HVF acceleration when available; TCG-only runs are recorded as such
  and never compared against accelerated runs.
- Host metadata (kernel, CPU, memory, QEMU version) stored next to the raw
  data.
- The guest size is currently capped by the kernel itself: the frame metadata
  is a static buffer, so the kernel refuses a guest with more than 1 GiB of
  RAM. Until the metadata becomes dynamic, both kernels must run with a
  guest size the kernel supports, and the same size for both.

### What a host without KVM means

A host without `/dev/kvm` runs QEMU in TCG mode, where the emulator
translates every guest instruction. Two consequences matter for the results:

- the absolute numbers describe the emulator, not the processor;
- a system call traps into the emulator, and that cost can dominate the
  difference between two kernels, hiding the effect being measured.

A TCG run is still a fair relative comparison, because both kernels run under
identical emulation with identical flags. What it cannot support is a claim
about hardware performance, and in particular the system call cost benchmark
has to be reported as "under emulation" rather than as a per-CPU-cycle figure.
Every report states the acceleration mode it was produced in.

A host with hardware acceleration is therefore required before any claim
about wall clock performance is published.

## The rig

`bench/run.py` boots a kernel image with a fixed configuration, hands it the
same initramfs, and reads the measurements from the serial line.

```sh
# both kernels
bench/run.py --madrust target/.../madrust-kernel --mainline /path/to/vmlinux
# MadRust only, before the mainline image exists
bench/run.py --madrust-only
```

The QEMU configuration is identical for both kernels:

```text
-M q35 -cpu Skylake-Client -smp 1 -m 512M \
-kernel <image> -initrd bench/initramfs.cpio \
-append "console=ttyS0" \
-display none -monitor none -no-reboot -serial file:<log>
```

One processor and 512 MiB, because the kernel is uniprocessor today and the
frame metadata describes 1 GiB at most. The CPU model is fixed on purpose:
`-cpu host` changes between machines and invalidates the comparison. Mainline
needs `console=ttyS0` so that its output reaches the same serial line; MadRust
ignores the command line. The initramfs is an uncompressed `cpio` archive,
which both kernels accept, so the bytes handed to the two kernels are
identical. It carries the program and the `/dev/console` and `/dev/null`
nodes, because a mainline kernel does not mount a device file system on an
initramfs.

### Workloads

One static Rust binary, built for the Linux musl target, holds every
measurement. It runs on both kernels without modification.

| Workload | What it measures |
|---|---|
| `syscall` | Cost of a system call entry and return, in a tight `getpid` loop |
| `mmap` | Cost of mapping and touching a region, per MiB |
| `futex` | Cost of a futex round trip between two threads |
| `threads` | Wall clock time of several threads doing a fixed amount of work |

The binary prints one `RESULT <workload> <value> <unit>` line per
measurement, and `BENCH DONE` at the end. A measurement that the kernel
refuses is printed with the error value, so an incomplete kernel produces
data instead of silence.

### Protocol

- At least 30 iterations per workload per kernel.
- Every iteration is recorded, including the ones that failed. Nothing is
  discarded, nothing is retried until it looks good.
- The raw data is one CSV per kernel, with one row per iteration: workload,
  iteration number, value, unit and status. The summary is a CSV with the
  statistics and a JSON file with the metadata.
- The summary reports the median, the 99th percentile and the standard
  deviation for every workload and every kernel, plus the metadata: the QEMU
  version, the host, the SHA-256 of both kernel images and the exact command
  line that produced the run.
- The clock inside the guest is the time stamp counter, calibrated against
  the timer interrupt. The timer alone ticks at 100 Hz, which would quantise
  every measurement to 10 ms and make the results meaningless.

### First baseline

The rig has produced MadRust only numbers so far, on a Mac under emulation
with a single processor. They are the baseline to compare against, not a
comparison:

```text
workload   median      p99       stddev   unit
syscall    231.00      297.00    23.90    ns/op
mmap       2221819.00  3654837.00  672448.19  ns/MiB
futex      3839.00     23517.00  3532.99  ns/roundtrip
threads    1478253.50  1695191.00 53820.39  ns/total
```

Every one of these numbers is an emulation artefact until the same workload
runs on mainline under the same flags. The raw rows are in
`bench/results/`.

### What the numbers mean

The comparison is between two kernels running the same binary under the same
emulation. It is not a comparison between a kernel and a distribution, and it
does not predict the performance of either on real hardware. The summary says
which acceleration mode produced the data, and a run without hardware
acceleration can only support a relative statement.

## Benchmarks

Kernel level:

- System call entry and return cost (tight `getpid` loop).
- Context switch latency (pipe ping-pong between two processes).
- Page fault cost (map and touch memory).
- `fork` and `exec` cost (busybox `true`).
- VFS metadata operations (create, stat, unlink in a tmpfs).
- `read`/`write` copy path bandwidth.

User level:

- `fio` on a ramdisk.
- `stress-ng` syscall and memory classes.
- `sysbench` thread and mutex tests.
- `unixbench` system index.
- A kernel build of `tinyconfig` as a large, honest end-to-end workload.
- Scheduling latency measured in the style of `cyclictest`, reported as
  scheduling latency only. MadRust is not a real-time kernel and makes no
  real-time claims.

## Where a win is plausible

At most two narrow claims, each with the losing configuration also published:

1. **System call cost in a trusted guest.** MadRust has no KPTI, no
   retpoline and no L1 flush on entry, in a single-tenant VM where the guest is
   trusted. Expect a measurable gap against default mainline, and a smaller
   gap against `mitigations=off`. Both numbers get published.
2. **Small-workload tail latency.** No LSM, no audit subsystem, no cgroups and
   no tracing hooks means less work per fork, exec and metadata operation.
   The expected win is in p99, not in throughput.

## Claims that are not made

No throughput claims about real applications, no networking claims, no
security claims ("more secure", "safer"), no "production ready", and no
comparison where one side runs features the other side does not have.

## Published artifacts

```text
bench/
  scripts/       the exact commands, parameterised and versioned
  results/       raw CSV per benchmark and run
  REPORT.md      the human-readable report, regenerated from the CSV
```

Each result set records the commit hash of both kernels, the QEMU version, the
host metadata and the full command lines used.
