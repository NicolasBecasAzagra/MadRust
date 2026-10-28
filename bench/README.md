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
madrust.qemu.log   what the emulator said, kept for the runs that fail
mainline.csv       the same for mainline, when it was run
mainline.log
summary.csv        median, p99 and standard deviation per workload
summary.json       the summary plus the run metadata
```

## The model and the agent

The rig also measures the runtime and the agent, with an initramfs that
carries the checkpoint:

```sh
bench/fetch-model.sh ~/madrust/artifacts/model-instruct
make agent
EXTRA_DIR=~/madrust/artifacts/model-instruct \
    bench/mkinitramfs.sh ~/madrust/artifacts/agent.cpio madrust-agent/madrust-agent.elf
bench/run.py --madrust target/.../madrust-kernel \
             --mainline artifacts/vmlinux \
             --initrd ~/madrust/artifacts/agent.cpio --memory 1024M \
             --only agent-steps,agent-tokens,agent-seconds,agent-step-ms
```

The agent prints one line per step and one summary line, and the rig turns
them into four measurements: the steps the loop took, the tokens the model
generated, the seconds the run lasted, and the milliseconds of a step. The
initramfs of the agent carries 270 megabytes of weights, so the guest needs
at least a gigabyte of memory.

## What an archive holds

`bench/initramfs-entries.py` prints every entry of an archive with its type,
its mode and, for a device, its major and minor numbers:

```sh
python3 bench/initramfs-entries.py bench/initramfs.cpio
```

It exists because of a failure that cost an afternoon: the archive was built
by a `cpio` that writes the name of an entry without the leading `./`, the
patch that turns the two empty files into device nodes did not match, and
`/dev/console` stayed a plain file. The program of the guest then wrote its
output into a file that nobody read, and the run looked like a guest that
says nothing. The rig now refuses an archive whose console is not the
console device, and this tool shows what the archive really holds.

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

The binary runs eight measurements, prints one `RESULT <workload> <value> <unit>`
line per measured quantity and sample, and ends with `BENCH DONE`. The rig
adds eight more rows of its own, taken from the emulator rather than from the
guest: the time to the first token and the tokens per second of the runtime,
the four measurements of the agent loop, and the wall clock and the resident
memory of the whole boot.

| Workload | What it measures | Unit |
|---|---|---|
| `syscall` | A tight `getpid` loop: system call entry and return | ns per call |
| `mmap` | `mmap` an anonymous region, touch every page, `munmap` | ns per MiB |
| `weights` | Map a large fixed region and touch every page | ns per MiB |
| `weights-tail` | The slowest single page of the `weights` sample | ns per page |
| `weights-huge` | The same region, asked for 2 MiB pages with `madvise` | ns per MiB |
| `weights-huge-tail` | The slowest single page of the `weights-huge` sample | ns per page |
| `futex` | Two threads ping-pong through one futex word | ns per round trip |
| `threads` | Four threads doing fixed arithmetic, spawn to join | ns for the whole run |

The `huge` variant asks the kernel for large pages with `madvise` and prints
the answer of the kernel to the advice, so a kernel without large pages says
so instead of being measured on something else.

`weights` stands in for faulting model weights in, which is the kernel cost that
dominates loading a model. MadRust has no file system yet, so the honest proxy
is the anonymous equivalent: a 256 MiB region mapped with `MAP_FIXED` at a fixed
address, with one byte read and written in every 4 KiB page. It is a proxy, not
a file-backed load, and the numbers have to be read as such. The region takes
half of the 512 MiB guest: the full 512 MiB cannot be faulted in when the kernel
and the initramfs live in the same memory.

Both kernels fault the region lazily, so the comparison is like for like: the
mapping reserves address space and the frames arrive when the program touches
them. `weights-tail` records the slowest single page of the walk, which is the
tail a server has to keep inside its latency budget. Every `weights` sample
prints both lines.

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

The sample counts are fixed in the binary: 30 samples for each of `syscall`,
`mmap`, `futex` and `threads`, 20 000 `getpid` calls per syscall sample, four MiB
per mmap sample, eight futex round trips per sample, and 200 000 arithmetic
steps per worker thread. `weights` prints five samples instead, because one
sample already touches 65 536 pages and five of them finish in a few seconds
under TCG; the harness knows the expected count per workload. They are large
enough to dominate the clock and small enough to finish in a few seconds under
TCG.

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

The boot is timed and recorded, as `boot-seconds` and `boot-rss-mb`, but it is
not a comparison of the kernel alone: the two kernels reach the first result
by different paths, and the reference kernel panics when its PID 1 leaves.
The number is published with that caveat, next to the measurements that are
comparisons.
