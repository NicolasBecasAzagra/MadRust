# MadRust: a Linux-ABI kernel in Rust, measured against mainline

**Repository**: <https://github.com/NicolasBecasAzagra/MadRust>
**Status**: milestones M0 to M4c, M8, M9, E1, E2, E2b, E3a, E3b, E4, E5 and E6 are implemented and
asserted in CI. The measurement below was produced by the rig in this
repository, from the raw rows in `bench/results/`.

## Abstract

MadRust is an x86_64 operating system kernel written entirely in Rust that
speaks the Linux userspace ABI. It boots a real language model, a 135 million
parameter Llama-architecture model, and generates the same tokens as the
reference implementation of that model, token for token, on MadRust and on
mainline Linux, from the same binary and the same initramfs.

The point of the project is not to be "a faster Linux". It is to measure what
a kernel costs an inference server, with a method that can be checked, and to
publish the losses as carefully as the wins. Three findings came out of it:

1. **With the mitigations disabled on both sides, MadRust is about two to ten
   times cheaper per operation** than mainline: 2.2 times per system call,
   4.4 times per megabyte faulted in with 4 KiB pages, 9.7 times with 2 MiB
   pages, 7.5 times per futex round trip. The difference is not the
   arithmetic of the program, which is the same binary on both sides, but the
   work the kernel does per operation.
2. **Large pages are an improvement on MadRust and not on the reference
   kernel**: mapping the weights with 2 MiB pages halves the cost per MiB and
   shortens the worst fault by more than an order of magnitude on MadRust,
   while the reference kernel gets slightly slower. The difference is not that
   one kernel has large pages and the other does not, it is what a large page
   costs when it is faulted in.
3. **The measurement that mattered most was the one we lost.** The first
   inference measurement showed MadRust twice as slow as the reference kernel,
   and the cause was specific: MadRust mapped a file with 4 KiB pages, so
   streaming 269 MB of weights took 65536 faults instead of 134. Fixing that
   took the faults to 791. The loss and its cause are published in
   `docs/BENCHMARKING.md` alongside the win.

## What is in the box

- **A kernel** (`madrust-kernel`): boots through the Xen PVH handover, runs
  from the higher half with a direct map of physical memory, manages physical
  memory with a buddy allocator whose metadata is carved out of the memory it
  describes, faults user pages in on demand, backs anonymous and file
  mappings with 2 MiB pages, serves 59 Linux system calls, exposes the
  initramfs as a read only file system, and runs preemptive kernel threads.
- **A runtime** (`madrust-infer`): reads a model in the `safetensors` format,
  maps it through the initramfs without copying it, and generates tokens with
  a Llama-style forward pass: RMSNorm, rotary embeddings, grouped query
  attention, the gated feed forward block, a key value cache and greedy
  sampling. It also holds a byte pair merge encoder, written from scratch, so
  a prompt built at run time reaches the model as the token ids it was
  trained with, and a byte level grammar that forces the answer to be a well
  formed tool call. No dependency outside the Rust standard library.
- **An agent** (`madrust-agent`): a loop of model, tool call, tool result and
  model, over five tools (`read_file`, `list_dir`, `write_file`, `compute` and
  `done`). The model chooses the tool and the arguments, the kernel decides
  how much of that costs.
- **A rig** (`bench/`): boots a kernel image with a fixed configuration,
  hands it the same initramfs, and records every sample, including the ones
  that failed. It rebuilds the kernel, the runtime and the initramfs on every
  run, and it refuses to report a measurement it could not take.

## Method

The rules were written before the first number was measured, in
`docs/BENCHMARKING.md`, and they were not adjusted afterwards.

- One workload binary, one initramfs, one QEMU configuration. Only the
  kernel image differs.
- The reference kernel is measured in two configurations: its default, and
  with `mitigations=off`. A default mainline kernel reloads CR3 on every
  system call to isolate the page tables, which under emulation invalidates
  the translator cache and is not a kernel property worth measuring on its
  own.
- At least 30 iterations per workload, every iteration recorded, failures
  recorded as failures.
- The median, the 99th percentile and the standard deviation for every
  configuration, plus the raw rows, the SHA-256 of both kernel images and the
  exact command line.
- The reference image is Linux 7.3.0 built with `CONFIG_PVH=y`, so that both
  kernels are started by the same handover with the same command line.

**Environment**: QEMU 11.1.2, TCG, one processor, 512 MiB of guest memory for
the operation benchmarks and 1024 MiB for the inference ones, on an arm64
macOS host. There is no hardware acceleration available on this host, which
is the most important limitation of everything below.

## Results: the cost of an operation

Two datasets, measured with the same rig and the same rules.

The **primary** one is a native x86_64 host: an Ubuntu server with six
cores, QEMU 8.2.2, and the reference kernel built from the mainline tree
by `bench/prepare-mainline.sh`. The **cross check** is the same rig on a
different host, an arm64 laptop with QEMU 11.1.2, where both kernels run
under emulation.

Medians, in the unit of the workload, on the native host:

| Workload | MadRust | mainline | mainline, mitigations off | MadRust against mainline with mitigations off |
|---|---|---|---|---|
| system call | 376.5 ns | 29.35 us | 1.04 us | **2.8x cheaper** |
| map and touch memory | 6.83 ms/MiB | 25.80 ms/MiB | 8.69 ms/MiB | **1.3x cheaper** |
| fault in memory, 4 KiB pages | 2.01 ms/MiB | 44.55 ms/MiB | 13.73 ms/MiB | **6.8x cheaper** |
| slowest single fault, 4 KiB | 1.74 ms | 42.60 ms | 38.89 ms | **22x cheaper** |
| fault in memory, 2 MiB pages | 824.8 us/MiB | 44.89 ms/MiB | 12.98 ms/MiB | **15.7x cheaper** |
| slowest single fault, 2 MiB | 189.1 us | 68.32 ms | 23.33 ms | **123x cheaper** |
| futex round trip | 6.53 us | 407.4 us | 97.71 us | **15x cheaper** |
| four threads, spawn to join | 619.5 us | 7.73 ms | 4.59 ms | **7.4x faster** |
| boot to the first result | 12.87 s | 206.29 s | 63.80 s | **5x faster** |
| resident memory of the guest | 319 MB | 476 MB | 483 MB | **1.5x less** |

The same rig on the cross check host:

| Workload | MadRust | mainline | mainline, mitigations off | MadRust against mainline with mitigations off |
|---|---|---|---|---|
| system call | 239.5 ns | 9827.5 ns | 516.0 ns | **2.2x cheaper** |
| map and touch memory | 2.72 ms/MiB | 8.34 ms/MiB | 3.23 ms/MiB | **1.2x cheaper** |
| fault in memory, 4 KiB pages | 950.1 us/MiB | 13.33 ms/MiB | 4.16 ms/MiB | **4.4x cheaper** |
| slowest single fault, 4 KiB | 263.5 us | 4.65 ms | 2.55 ms | **9.7x cheaper** |
| fault in memory, 2 MiB pages | 445.5 us/MiB | 14.01 ms/MiB | 4.33 ms/MiB | **9.7x cheaper** |
| slowest single fault, 2 MiB | 28.2 us | 4.80 ms | 1.67 ms | **59x cheaper** |
| futex round trip | 3.70 us | 136.7 us | 27.7 us | **7.5x cheaper** |
| four threads, spawn to join | 228.0 us | 1536.0 us | 970.0 us | **4.3x faster** |

The two hosts agree on the order of the effect and disagree on its size,
which is what a measurement under emulation does. The fault rows move the
most: a fault costs the emulator, and the emulator pays more for the work
mainline does per fault. The full tables with the 99th percentile and the
standard deviation are in `bench/results/summary.csv` and in
`bench/results/vps-1cpu/`.

Against the default reference configuration, which pays for page table
isolation on every system call, the system call row is 78x on the native
host. That number is published but it is not a kernel comparison, and the
report says so.

Where the difference comes from, in the order the size of the effect:

- **No page table isolation, no LSM or auditing hooks, no virtual memory
  accounting.** A system call that does not have to walk an audit path or
  reload CR3 is cheaper.
- **Demand paging.** A mapping reserves address space and the frames arrive
  when the program touches them, so a program that reads one byte of a 256 MiB
  mapping does not pay for the rest of it. This single change made the
  thread workload seven times faster and is the reason the thread row is a
  win at all: before it, MadRust was slower there.
- **A simpler futex path.** Eight waiters in a flat table, no robust list,
  no requeue.
- **Large pages for anonymous and for file mappings**, with the cost of the
  copy documented.

## Results: large pages

On the native host:

| | 4 KiB pages | 2 MiB pages |
|---|---|---|
| MadRust, cost per MiB | 2.01 ms | 824.8 us |
| MadRust, slowest fault | 1.74 ms | 189.1 us |
| mainline with mitigations off, cost per MiB | 13.73 ms | 12.98 ms |
| mainline with mitigations off, slowest fault | 38.89 ms | 23.33 ms |

And on the cross check host:

| | 4 KiB pages | 2 MiB pages |
|---|---|---|
| MadRust, cost per MiB | 950.1 us | 445.5 us |
| MadRust, slowest fault | 263.5 us | 28.2 us |
| mainline with mitigations off, cost per MiB | 4.16 ms | 4.33 ms |
| mainline with mitigations off, slowest fault | 2.55 ms | 1.67 ms |

The raw rows of the first measurement of the large pages are in
`bench/results/e2/`.

The reference kernel gets slightly slower per MiB when it uses large pages,
and its worst fault does not improve. On MadRust the same change halves the
cost per MiB and shortens the worst fault by a factor of nine. Both kernels
have large pages; they do not cost the same to fault in.

The reason is what happens when a large page arrives: the folio has to be
allocated, zeroed and accounted for, and the compaction machinery around it
is not free. MadRust allocates a 2 MiB block from its own buddy allocator,
zeroes it and maps it.

For a file mapping, the data of the file is not aligned to 2 MiB in physical
memory, so the kernel copies it into a large page once, the way the read only
huge pages of a file system do. That doubles the memory a mapped file
occupies, and the cost is in the report.

## Results: the arithmetic, measured against the reference

The kernel part of the story is measured by the rig, which boots two kernels.
The arithmetic part is measured separately, natively, because that is the
comparison a user makes: not our kernel against another kernel, but our
program against the programs people run. Both programs run natively on the
same host, with the same weights, and they produce **the same thirty two
tokens**, so what is compared is the arithmetic.

| Natively, one host, one checkpoint | tokens per second | time to the first token |
|---|---|---|
| MadRust runtime, Rust, no dispatcher and no autograd | **23.6** | **286 ms** |
| Reference implementation, eager, float32 | 10.9 | (2.93 s for 32 tokens) |

A specialised forward pass written in Rust, with blocked matrix multiplication
and the vector instructions of the host, is 2.2 times faster than the eager
path of the reference implementation on this model, and it pins the weights in
about seventy milliseconds, which is inside the time to the first token.

The claim is narrow on purpose. The reference runs in eager mode, so its
dispatcher and its autograd are part of what it costs; a compiled or quantized
reference would be a different comparison. The host varies by around thirty
per cent between runs, so this is a sample and not a precise measurement. The
raw values are in `bench/results/native/`.

## Results: serving a model

The runtime serves SmolLM2-135M, revision
`93efa2f097d58c2a74874c7e644dbc9b0cee75a2`, Apache-2.0, 269 MB of bfloat16
weights. It generates `260 3575 282 260 1798 30 198 198` and the text
"The capital of France is the capital of the country.", which is exactly what
the reference implementation produces for the same prompt, in float32, on an
ordinary Linux host. The reference run is in `bench/reference/`, and the
runtime is checked against it.

**The measurement that does not depend on the host is the number of faults.**
Streaming the weights used to take 65536 faults of 4 KiB. With large pages
for file mappings it takes 791, of which 127 are large pages: a factor of
eighty three, and the same on every run. The runtime now also pins the
weights, and the kernel confirms it: 65688 pages of 269058048 bytes are
resident, pinned in about 96 milliseconds, and that cost is inside the time to
the first token.

**The timings do not support a ratio.** Under emulation the same build
measures between 9.8 and 22.2 seconds to the first token depending on where
the threads land. Three batches of the same measurement are published in
`bench/results/inference/`, `bench/results/inference-before/` and
`bench/results/inference-joint/`:

```text
                         MadRust    mainline    mainline (mitigations=off)
batch 1, before           21.20 s      23.34 s              9.96 s
batch 2, after            22.17 s      22.83 s             21.92 s
joint batch               10.70 s         -                    -
```

In the first batch the reference kernel without mitigations measured 9.9
seconds and in the second 21.9, so the spread between two runs of the same
build under emulation is larger than the difference between the kernels, and
no ratio can be published from those batches.

**After the arithmetic and the residency were added, the gap is larger than
the variance.** In the batch that is committed, the time to the first token on
MadRust was 10.70 seconds and the rate 0.52 tokens per second, against 21.9
and 22.8 seconds for the reference kernel in the batches above. A gap of more than three times
survives a variance of two, so this one can be stated. What it measures is a
kernel that faults the weights in large pages, keeps them resident, and runs
a forward pass written in Rust without a single dependency outside the
standard library.

What still cannot be said:

- **The absolute numbers describe an emulator.** They say how the two kernels
  compare under the same translation overhead, not how fast either is on a
  processor.
- **The runtime is deliberately naive.** It is a matrix multiplication in
  plain Rust, without a library of vector kernels, so the absolute rate is
  not what a tuned implementation reaches. The comparison that isolates the
  arithmetic is against the reference implementation on the same host, and
  that number is measured separately.

## Results: the agent

The agent is the last piece of the claim: a model that a program asks what to
do, a tool that does it, and the model again. It runs on the same kernel, with
the same runtime, with the same checkpoint and with the same binary as the
reference kernel.

The checkpoint is `HuggingFaceTB/SmolLM2-135M-Instruct`, revision
`12fd25f77366fa6b3b4b768ec3050bf629380bac`, Apache-2.0. The agent has five
tools (`read_file`, `list_dir`, `write_file`, `compute` and `done`) and the
model chooses which one to call and with which arguments. The grammar of the
call is not a prompt instruction but an automaton over bytes: the answer of the
model cannot be anything but a well formed call of a declared tool, carrying
the keys that tool needs, each of them once, with the type the tool expects.

The task is to read `/vocab.json` from the initramfs and report its size. On
the native host, one processor, 1024 MiB, the same initramfs for both kernels:

| | MadRust | mainline | mainline, mitigations off |
|---|---|---|---|
| Steps of the loop | 2 | 2 | 2 |
| Tokens the model generated | 39 | 39 | 39 |
| Seconds of the loop | **821.0** | 2792.7 | 2570.1 |
| Seconds of a step | **410.5** | 1396.4 | 1285.0 |
| Boot to the last result | **1200.8 s** | 2807.6 s | 2584.3 s |
| Resident memory of the guest | **857 MB** | 1021 MB | 1023 MB |

The loop is three times faster on MadRust than on the reference kernel with the
same binary and the same checkpoint, and the guest needs 164 megabytes less.
The raw rows are in `bench/results/agent/`. The rows of the runtime on the
same host are in `bench/results/inference-native/`, and the rows of the
operations are in `bench/results/vps-1cpu/`.

The transcript of the run is the honest part:

```text
AGENT step=1 tool=read_file args={"path":"/vocab.json"} result=bytes=800662 read=4096 content=…
AGENT step=2 tool=read_file args={"path":"/vocab.json"} result=the model repeated the call
AGENT steps=2 tokens=39 seconds=821.017 tool_calls=2
```

The model of 135 million parameters reads the file and answers with its real
size, 800662 bytes, and then asks for the same file again. The loop notices
the repetition and stops there instead of spending the rest of its budget on
the same call. A model that knew when to stop would answer instead of asking
again; that is a property of the checkpoint, not of the kernel, and the number
the kernel is responsible for is the one in the table.

## Losses

- **The thread workload was a loss and then became a win.** With eager
  allocation of the mappings, MadRust was 1.5 times slower than the reference
  kernel there. Demand paging made it four times faster. The loss was
  published at the time, in `bench/results/demand-paging/`.
- **One boot of the reference kernel did not finish** in an earlier run and
  the rig recorded the twenty seven missing samples instead of hiding them.
  A repeated run completed all thirty iterations, so it was a flake. Both
  runs are in `bench/results/anomaly/` and `bench/results/repeat/`.
- **The first reference image could not be asked for large pages at all**:
  `madvise(MADV_HUGEPAGE)` returned `EINVAL`, because that advice is only
  valid when the kernel is built with transparent huge pages. The workload
  reports what the kernel answers, with `MADV_NORMAL` as the control, so the
  difference was found by a probe and not by reading a configuration file.

## Threats to validity

- **There is no hardware acceleration on this host.** Every absolute number
  describes an emulator. The ratios between the kernels are less affected,
  because both pay the same translation cost, but the translation cost is
  large compared to the work being measured.
- **The measurement variance under emulation is large**, up to a factor of
  two for the inference timings, and it is larger than the difference
  between the kernels there.
- **The clock source differs.** The reference kernel reads the time stamp
  counter through the vDSO, MadRust through a system call, which costs
  MadRust one system call per measurement point. The measurements that
  timestamp every sample are biased against MadRust.
- **One processor, 512 MiB.** A server that batches requests is not that.
- **The runtime is deliberately naive.** It is a matmul in plain Rust with
  no BLAS and no vectorisation, so that the comparison is about the kernel.
  The arithmetic is identical on both sides, but the absolute tokens per
  second are not what a real inference stack would reach.
- **The workload is ours.** It measures the operations the kernel does while
  serving a model, not the model.

## Reproducing it

```sh
# the kernel and the runtime
make build
make run                      # boots and prints the memory map and the ticks
make run-infer                # boots with the inference runtime

# the comparison
bench/prepare-mainline.sh     # on a Linux host: the reference image
bench/mkinitramfs.sh
python3 bench/run.py \
    --madrust "$(make -s print-kernel-path)" \
    --mainline ~/madrust/artifacts/vmlinux \
    --mainline-variant "nomitig=mitigations=off" \
    --out bench/results

# the checkpoint, verified
bench/fetch-model.sh ~/madrust/artifacts/model-instruct

# the agent
make agent
EXTRA_DIR=~/madrust/artifacts/model-instruct \
    bench/mkinitramfs.sh ~/madrust/artifacts/agent.cpio madrust-agent/madrust-agent.elf
python3 bench/run.py \
    --madrust "$(make -s print-kernel-path)" \
    --mainline ~/madrust/artifacts/vmlinux \
    --initrd ~/madrust/artifacts/agent.cpio --memory 1024M \
    --only agent-steps,agent-tokens,agent-seconds,agent-step-ms \
    --out bench/results/agent

# the oracle for the runtime
python3 bench/reference/reference_tokens.py --out reference.json
```

Every assertion in the continuous integration runs the kernel and checks the
serial log, so the claims in this document are checked on every push.

## What comes next

- **A host with hardware acceleration**, which is the only thing missing for
  the inference timings to mean something about a machine rather than about
  an emulator.
- **A scheduler with a better tail, and more than one processor.** A server
  that batches requests lives on the latency of the tail, and the kernel is
  still uniprocessor.
- **The same method applied to a real serving stack**, with a model that is
  actually used, rather than one that is served to see what it costs.

## The method, for anyone else

The kernel was written with the assistance of an AI agent, against milestones
whose exit criteria are machine checked: a serial log that has to contain
exact lines, a unit test on the host, a row in the benchmark data. The
design decisions, the criteria and the verification belong to a human; the
code was written by an agent against them.

That is the part that may be worth copying. "How much of an operating system
can be written this way, and how do you know it works" is a question worth an
answer, and this repository is one answer to it, with the losses published
next to the wins.
