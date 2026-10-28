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
5. **The boot is recorded, and it is not a comparison of the kernel.** The two
   kernels reach the first result by different paths, and the reference kernel
   panics when its PID 1 leaves, so the wall clock of the boot is published
   with that caveat and never read as a kernel property. The workload
   measurements are the ones that compare.
6. **Mitigation configurations are always published.** Mainline Linux is
   measured twice: with its default configuration, and with
   `mitigations=off`. Reporting only the default, or only the mitigated-off
   run, is cherry-picking.
7. **The number of samples is fixed by the workload.** Thirty for the four
   operation workloads, five for `weights`, ten for `weights-huge`, and one for
   the model, the agent and the boot rows. Report median, p99 and standard
   deviation. Commit the raw per-iteration data as CSV.
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
- The guest size is capped by the kernel itself: the frame metadata is carved
  out of the memory it describes, so the kernel refuses a guest with more than
  32 GiB of RAM. Both kernels must run with a guest size the kernel supports,
  and with the same size for both.

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
| `weights-huge` | The same region, asked for 2 MiB pages with `madvise`, per MiB |
| `weights-huge-tail` | The slowest single page fault of that region, in nanoseconds |
| `infer-ttft` | Time to the first token of the runtime, with the real checkpoint |
| `infer-tps` | Tokens per second of the runtime, after the first one |
| `agent-steps` | Steps the agent loop took |
| `agent-tokens` | Tokens the model generated in the whole agent run |
| `agent-seconds` | Wall clock time of the agent run |
| `agent-step-ms` | Milliseconds of one step of the agent loop |
| `boot-seconds` | Wall clock time from the start of the emulator to the last line |
| `boot-rss-mb` | Resident memory the emulator reached |
| `net` | Time of a round trip over the network: a datagram to a server on the host and the answer back |

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

- Thirty iterations for the four operation workloads per kernel, five for
  `weights`, ten for `weights-huge`, thirty for `net`, and one for the model,
  the agent and the boot rows.
- Every iteration is recorded, including the ones that failed. Nothing is
  discarded, nothing is retried until it looks good.
- The raw data is one CSV per kernel, with one row per iteration: workload,
  iteration number, value, unit and status. The summary is a CSV with the
  statistics and a JSON file with the metadata.
- The summary reports the median, the 99th percentile and the standard
  deviation for every workload and every kernel, plus the metadata: the QEMU
  version, the host, the SHA-256 of both kernel images and the exact command
  line that produced the run.
- The reference kernel panics when PID 1 exits, and the power off of the
  guest does not always reach the machine, so a reference run does not always
  end on its own. The rig stops the guest as soon as every measurement it
  was asked for is in the log, and takes the time of that moment as the time
  of the run. The panic that follows is the reference kernel's behaviour and
  it is published in the log.
- The time of a boot is the moment of the last line of the workload, not the
  moment the emulator process ends. A run that reaches the timeout reports an
  error instead of a number: the timeout is not a measurement of a boot, and
  publishing it as one would turn a failure into a result.
- The output of the emulator itself is kept in a file of its own
  (`<kernel>.qemu.log`) and printed when a run fails. When a guest does not
  boot, the reason is in that stream and not in the serial log of the guest,
  so discarding it hides the only explanation there is.
- The clock inside the guest is the time stamp counter, calibrated against
  the timer interrupt. The timer alone ticks at 100 Hz, which would quantise
  every measurement to 10 ms and make the results meaningless.

### First results

MadRust against Linux 7.3.0, one processor, 512 MiB, thirty iterations per
workload, under emulation with no hardware acceleration. Both kernels fault
the measured regions lazily, so the comparison is like for like.

```text
workload           MadRust     mainline    mainline (mitigations=off)
syscall            239.5 ns      9827 ns            516 ns
mmap             2.723 ms/MiB   8.341 ms/MiB     3.226 ms/MiB
weights            950087 ns/MiB 13.332 ms/MiB    4.156 ms/MiB
weights-tail         263525 ns    4649782 ns      2554257 ns
weights-huge        445463 ns/MiB 14.005 ms/MiB    4.332 ms/MiB
weights-huge-tail     28234 ns    4799645 ns      1673184 ns
futex                 3.70 us      136.7 us          27.69 us
threads              228.0 us      1536.0 us         970.0 us
```

Median values, thirty samples each except the tail measurements, which have
five and ten. The 99th percentile and the standard deviation of every
configuration are in `bench/results/summary.csv`, and every raw iteration is in
the per kernel CSV files next to it.

**What these numbers are.** The default mainline column is not a kernel
comparison: a default mainline kernel reloads CR3 on every system call to
isolate the page tables, and under emulation that flush invalidates the
translator cache, which is why the number is two orders of magnitude away.
The honest comparison is the third column, where the reference kernel runs
with the same mitigations disabled that MadRust never implements.

**What the comparison supports.** With the mitigations disabled on both
sides, MadRust is about twice as cheap per system call, a fifth as expensive
per MiB faulted in with 4 KiB pages, a tenth as expensive per MiB faulted in
with 2 MiB pages, seven times as cheap per futex round trip and four times as
fast in the thread workload. The tails are further apart still: the slowest
single fault is 264 microseconds on MadRust against 2.55 milliseconds on the
reference, and with 2 MiB pages 28.2 microseconds per fault against 1.67
milliseconds.

Those differences come from doing less work per operation: no page table
isolation machinery, no LSM and auditing hooks, no virtual memory accounting,
a much simpler futex path, and demand paging that does not touch a page the
program never reads.

**The finding about large pages.** In the reference kernel, large pages are
not an improvement under emulation: 14.0 ms per MiB with 2 MiB pages against
13.3 ms with 4 KiB pages. The cost of the big folio, of zeroing it and of the
compaction bookkeeping eats the benefit of the fewer faults. On MadRust the
same change is an improvement: 950 microseconds per MiB down to 445, and a tail
two orders of magnitude shorter. The difference is therefore not that one
kernel has large pages and the other does not. It is what a large page costs
when it is faulted in.

**What the comparison does not support.** The two tail measurements have five
and ten samples, so their 99th percentile is barely a percentile. The ratios
are inflated by the overhead the emulator adds to both kernels, because every
guest instruction is translated. The clock source differs as well: mainline
reads the time stamp counter through the vDSO, MadRust through a system call,
which costs MadRust one system call per measurement point. One processor with
512 MiB is a small machine, and the fast metrics move by around thirty per
cent between runs of the same binary on the same host.

**An anomaly that is published.** In an earlier run, one of the thirty boots
of the reference kernel with the mitigations disabled did not finish, and the
rig recorded the twenty seven missing samples instead of hiding them. A
repeated run of the same configuration completed all thirty iterations, so it
was a flake and not a difference between the kernels. Both runs are in the
repository: `bench/results/anomaly/` is the run with the missing samples and
`bench/results/repeat/` is the repetition.

### Large pages

Mapping the weights with 2 MiB pages instead of 4 KiB ones is the change that
moves the cost of loading a model the most, and the rig measures both regimes
on every kernel:

```text
                          MadRust     mainline    mainline (mitigations=off)
cost per MiB, 4 KiB          950087 ns 13.332 ms      4.156 ms
cost per MiB, 2 MiB          445463 ns 14.005 ms      4.332 ms
slowest fault, 4 KiB         263525 ns 4649782 ns    2554257 ns
slowest fault, 2 MiB          28234 ns 4799645 ns    1673184 ns
```

On MadRust the large pages halve the cost per MiB and shorten the worst
fault by more than an order of magnitude, because 512 faults become one and
the fault that remains is a single large allocation. In the reference kernel
the same change makes the cost per MiB slightly worse and leaves the tail
where it was: the price of the large folio, of zeroing it and of the
bookkeeping around it eats the benefit of the fewer faults.

The `weights-huge-tail` figure is the slowest fault divided by the 512 pages
it covers, so it is comparable with the 4 KiB figure.

### A capability difference the rig found

The workload asks the kernel for large pages with `madvise(MADV_HUGEPAGE)`
and reports what the kernel answers, with `MADV_NORMAL` as the control:

```text
a reference build without transparent huge pages:
    ADV madvise-normal=0 madvise-hugepage=-22
a reference build with them:
    ADV madvise-normal=0 madvise-hugepage=0
MadRust:
    ADV madvise-normal=0 madvise-hugepage=0
```

In the Linux source, `MADV_HUGEPAGE` is only a valid advice when
`CONFIG_TRANSPARENT_HUGEPAGE` is enabled, so a reference build without it
refuses the request with `EINVAL` and the large page regime cannot be
compared at all. The first reference image this project used was built that
way, and the difference was found by the probe rather than by reading the
configuration. `bench/prepare-mainline.sh` now enables
`CONFIG_TRANSPARENT_HUGEPAGE` in the reference build, so the comparison runs
against a kernel that has the feature.

### The inference measurement

The rig also measures the runtime: it boots the initramfs that carries the
model, and records the time to the first token and the tokens per second for
every kernel, with the same binary and the same flags.

The measurement that does not depend on the host is the number of page
faults. Streaming 269 megabytes of weights used to take 65536 faults of 4 KiB;
with large pages for file mappings it takes 791, of which 127 are large pages.
That is the work the kernel does, and it is the same on every run.

The timings are a different matter. Under emulation the same build measures
between 9.8 and 22.2 seconds to the first token depending on where the
threads land, so the numbers describe the emulator and the host, not the
kernel. Three batches of the same measurement:

```text
                         MadRust    mainline    mainline (mitigations=off)
batch 1, before           21.20 s      23.34 s              9.96 s
batch 2, after            22.17 s      22.83 s             21.92 s
joint batch               10.70 s         -                    -
```

What can be said, and what cannot:

- **The fault count is the claim.** 65536 faults against 791 is a factor of
  eighty three, and it does not move between runs. On a host where a fault
  has a fixed price, that is time.
- **No ratio can be published for the timings.** The spread between two runs
  of the same build under emulation is larger than the difference between
  the kernels, and the third batch shows it: there, the reference kernel
  without mitigations measured 21.9 seconds and in the first batch 9.9.
  Reading a ratio out of that would be reading noise.
- **The comparison needs a host with hardware acceleration**, where a fault
  costs a predictable amount and the measurement can be repeated. Until then,
  the inference numbers are published as a range with the caveat attached,
  and the fault count carries the argument.

The raw rows of the three batches are in `bench/results/inference/`,
`bench/results/inference-before/` and `bench/results/inference-joint/`.

### The checkpoint

The inference runs serve a real checkpoint, so the checkpoint is part of the
methodology and not a detail of the setup. `bench/fetch-model.sh` downloads
`HuggingFaceTB/SmolLM2-135M-Instruct` at a pinned revision and checks the
SHA-256 of every file before a run uses it:

| File | Bytes | Role |
|---|---|---|
| `model.safetensors` | 269060552 | The weights, bfloat16 |
| `vocab.json` | 800662 | The vocabulary, token to id |
| `config.json` | 861 | The dimensions of the model |
| `tokenizer.json` | 2104556 | The tokenizer, with the merges inside |
| `tokenizer_config.json` | 3764 | The chat template and the special token ids |
| `merges.txt` | 466391 | The merges, for a byte pair merge encoder |

The revision is pinned, because a moving reference would change the weights
under a published measurement. A weight file that arrived incomplete would
produce numbers that mean nothing and a wrong answer much later, so the
digest is checked before the file is used and the run stops when it does not
match.

The chat markers of the template are single tokens of the vocabulary, so a
prompt can carry them as one token each. The runtime has no byte pair merge
encoder, so the plain text of a prompt is encoded one byte per token: the
model reads the same bytes, with more tokens to carry them.

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
