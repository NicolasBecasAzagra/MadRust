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

One processor and 512 MiB, because the kernel is uniprocessor today. The
kernel can describe up to 32 GiB of guest memory, so a larger guest is
possible, but the number of processors is a property of the kernel and not of
the rig. The CPU model is fixed on purpose: `-cpu host` changes between
machines and invalidates the comparison. Mainline needs `console=ttyS0` so
that its output reaches the same serial line; MadRust ignores the command
line. The initramfs is an uncompressed `cpio` archive, which both kernels
accept, so the bytes handed to the two kernels are identical. It carries the
program and the `/dev/console` and `/dev/null` nodes, because a mainline
kernel does not mount a device file system on an initramfs.

### Workloads

One static Rust binary, built for the Linux musl target, holds every
measurement. It runs on both kernels without modification.

| Workload | What it measures |
|---|---|
| `syscall` | Cost of a system call entry and return, in a tight `getpid` loop |
| `mmap` | Cost of mapping and touching a region, per MiB |
| `futex` | Cost of a futex round trip between two threads |
| `threads` | Wall clock time of several threads doing a fixed amount of work |
| `weights` | Cost of faulting in a 256 MiB region, per MiB |
| `weights-tail` | The slowest single page fault of that region, in nanoseconds |

The binary prints one `RESULT <workload> <value> <unit>` line per
measurement, and `BENCH DONE` at the end. A measurement that the kernel
refuses is printed with the error value, so an incomplete kernel produces
data instead of silence.

`weights` and `weights-tail` are the proxy for the dominant kernel cost of
loading a model: there is no file system yet, so the anonymous mapping is
the honest equivalent, mapping a fixed 256 MiB region and touching one byte
in every page. The region is fixed at an address and the sample count is
five, because every sample faults 65536 pages.

### Protocol

- At least 30 iterations per workload per kernel, and five for the two
  workloads that fault 65536 pages each.
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

### First results

MadRust against Linux 7.3.0, one processor, 512 MiB, thirty iterations per
workload, under emulation with no hardware acceleration. Both kernels fault
the measured regions lazily, so the comparison is like for like.

```text
workload        MadRust     mainline    mainline (mitigations=off)
syscall         245.5 ns      9581 ns            520 ns
mmap          1.768 ms/MiB   7.792 ms/MiB     3.263 ms/MiB
weights       907.6 us/MiB  12.700 ms/MiB     4.117 ms/MiB
weights-tail  474.8 us/page  1293.4 us/page   572.9 us/page
futex           3.56 us      128.5 us          24.06 us
threads        224.7 us      1566.5 us        823.5 us
```

Median values, thirty samples each except the two tail measurements, which
have five. The 99th percentile and the standard deviation of every
configuration are in `bench/results/summary.csv`, and every raw iteration is in
the per kernel CSV files next to it.

**What these numbers are.** The default mainline column is not a kernel
comparison: a default mainline kernel reloads CR3 on every system call to
isolate the page tables, and under emulation that flush invalidates the
translator cache, which is why the number is two orders of magnitude away.
The honest comparison is the third column, where the reference kernel runs
with the same mitigations disabled that MadRust never implements.

**What the comparison supports.** With the mitigations disabled on both
sides, MadRust is about twice as cheap per system call, about twice as cheap
per MiB mapped and touched, four and a half times as cheap per MiB faulted in,
seven times as cheap per futex round trip, and three and a half times as fast
in the thread workload. Those differences come from doing less work per
operation: no page table isolation machinery, no LSM and auditing hooks, no
virtual memory accounting, a much simpler futex path, and demand paging that
does not touch a page the program never reads.

**What the comparison does not support.** The two tail measurements have five
samples each, so their 99th percentile is barely a percentile and their
medians move by tens of percent between runs. The ratios are inflated by the
overhead the emulator adds to both kernels, because every guest instruction
is translated. The clock source differs as well: mainline reads the time
stamp counter through the vDSO, MadRust through a system call, which costs
MadRust one system call per measurement point. And one processor with 512 MiB
is a small machine.

**An anomaly that is published.** In an earlier run, one of the thirty boots
of the reference kernel with the mitigations disabled did not finish, and the
rig recorded the twenty seven missing samples instead of hiding them. A
repeated run of the same configuration completed all thirty iterations, so it
was a flake and not a difference between the kernels. Both runs are in the
repository: `bench/results/anomaly/` is the run with the missing samples and
`bench/results/repeat/` is the repetition.

### Large pages

Mapping the weights with 2 MiB pages instead of 4 KiB ones is the change that
moves the cost of loading a model the most, and the rig measures both
regimes on the same kernel:

```text
                          MadRust, 4 KiB    MadRust, 2 MiB
cost per MiB                    896593 ns        435933 ns
slowest single fault           277732 ns         15646 ns
```

Twice the throughput and a tail eighteen times shorter, because 512 faults
become one. The `weights-huge-tail` figure is the slowest fault divided by the
512 pages it covers, so it is comparable with the 4 KiB figure.

These numbers are from the MadRust side only and are recorded in
`bench/results/e2/`. The comparison against mainline has to be run again: the
reference image was lost with the volume it was on, and the published
comparison in `bench/results/` was measured with the kernel of the previous
milestone.

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
