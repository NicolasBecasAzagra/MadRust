/**
 * Every fact of the site lives here, so the numbers in the pages are the
 * numbers of the repository. Nothing is written twice: the benchmark table is
 * the table of README.md, the milestones are the ladder of docs/ARCHITECTURE.md.
 */

export const meta = {
  name: 'MadRust',
  tagline: 'An operating system for local AI agents, written in Rust from the kernel up.',
  description:
    'MadRust is a from-scratch x86_64 kernel that speaks the Linux userspace ABI. On top of it sit an inference runtime with no dependency outside the Rust standard library, and an agent that asks the model what to do and runs the tools.',
  repo: 'https://github.com/NicolasBecasAzagra/MadRust',
  repoPath: 'NicolasBecasAzagra/MadRust',
  license: 'GPL-2.0-only',
  licenseUrl: 'https://github.com/NicolasBecasAzagra/MadRust/blob/main/LICENSE',
  author: 'Nicolas Becas Azagra',
  toolchain: 'nightly-2026-10-24',
  emu: 'QEMU 8.2.2',
  reference: 'Linux 7.3.0',
  status: 'In development. Not production ready.',
} as const;

export type Layer = {
  id: string;
  name: string;
  role: string;
  body: string;
  crates: string[];
  differentiator: string;
};

export const layers: Layer[] = [
  {
    id: 'kernel',
    name: 'The kernel',
    role: 'A kernel that does less',
    body: 'No page table isolation, no LSM, no audit, no cgroups, no hooks. Demand paging, large pages, a scheduler with a short tail and real multiprocessing. Every number is published, including the losses.',
    crates: [
      'madrust-kernel',
      'madrust-arch',
      'madrust-mm',
      'madrust-sched',
      'madrust-elf',
      'madrust-initramfs',
      'madrust-bootinfo',
      'madrust-drivers',
    ],
    differentiator: 'Fewer milliseconds of work per operation, in a private single tenant guest.',
  },
  {
    id: 'runtime',
    name: 'The runtime',
    role: 'A runtime that serves a model',
    body: 'The weights are mapped from the initramfs without being copied, kept resident, and read by a decoder written from scratch: RMSNorm, rotary embeddings, grouped query attention, the gated feed forward block and a key value cache. It generates the same tokens as the reference implementation, one for one.',
    crates: ['madrust-infer', 'madrust-initramfs'],
    differentiator: 'No dependency outside the Rust standard library, and no copy of the weights.',
  },
  {
    id: 'agent',
    name: 'The agent',
    role: 'An agent that acts',
    body: 'The model answers with a tool call, the kernel runs the tool, and the model answers again. The shape of the call is not a request in the prompt but an automaton over bytes: the answer cannot be anything but a well formed call of a declared tool, with the keys that tool needs, each of them once, with the type the tool expects.',
    crates: ['madrust-agent', 'madrust-http'],
    differentiator: 'A grammar that makes a malformed tool call impossible.',
  },
  {
    id: 'network',
    name: 'The network',
    role: 'The fourth layer',
    body: 'The guest enumerates the bus, claims a network device, resolves the address of the gateway, sends a datagram to a server on the host and reads the answer, with the socket calls of the Linux ABI. The agent has a tool that does it, so the loop of the agent reaches the network.',
    crates: ['madrust-net', 'madrust-netclient', 'madrust-netstream', 'madrust-http'],
    differentiator: 'A stack sized for the agent loop, not for the general case.',
  },
];

export const agentTools = [
  { name: 'read_file', what: 'Read a file of the machine.' },
  { name: 'list_dir', what: 'List a directory.' },
  { name: 'write_file', what: 'Write a file.' },
  { name: 'compute', what: 'Run a computation.' },
  { name: 'net_send', what: 'Send a datagram over UDP.' },
  { name: 'http_get', what: 'Call an API over HTTP.' },
  { name: 'done', what: 'End the loop with an answer.' },
];

export type MilestoneState = 'done' | 'next' | 'deferred';

export type Milestone = {
  id: string;
  name: string;
  delivers: string;
  state: MilestoneState;
  exit?: string;
  verification?: string;
};

export const milestones: Milestone[] = [
  {
    id: 'M0',
    name: 'Boot',
    delivers: 'Xen PVH handover, serial output, physical memory map.',
    state: 'done',
    exit: 'Serial log contains "MadRust: boot OK" and QEMU exits 33.',
    verification: 'Headless QEMU with a serial assertion.',
  },
  {
    id: 'M1',
    name: 'Exceptions and timers',
    delivers: 'GDT, TSS with an IST stack, IDT, recoverable exceptions, 8259 PIC, PIT.',
    state: 'done',
    exit: 'Divide by zero prints "DIV0 RIP=..." and continues; 100 PIT ticks print "TICKS 100".',
    verification: 'Golden serial log.',
  },
  {
    id: 'M2',
    name: 'Memory',
    delivers: 'Buddy allocator, size class heap, higher half kernel, direct map.',
    state: 'done',
    exit: '10,000 allocate/free cycles without leaks or double frees; usable RAM matches -m.',
    verification: 'Host unit tests plus an on target self test.',
  },
  {
    id: 'M2b',
    name: 'Direct map only',
    delivers: 'The kernel reaches physical memory only through the direct map.',
    state: 'done',
    exit: 'The same byte read before and after the switch.',
    verification: 'On target self test.',
  },
  {
    id: 'M2c',
    name: 'Metadata in its own memory',
    delivers: 'Frame metadata carved out of the memory it describes, guests of 32 GiB.',
    state: 'done',
    exit: 'A guest of 4 GiB boots, manages its memory and runs the workload.',
    verification: 'QEMU with -m 4G.',
  },
  {
    id: 'M3',
    name: 'Threads',
    delivers: 'Kernel threads, preemptive round robin scheduler, interrupt safe locks.',
    state: 'done',
    exit: '10 threads make progress under preemption with fair scheduling.',
    verification: 'On target self test.',
  },
  {
    id: 'M4',
    name: 'Userspace',
    delivers: 'Ring 3, SYSCALL/SYSRET, ELF64 loader, 59 Linux system calls.',
    state: 'done',
    exit: 'A statically linked hello prints and exits 0; the system calls return the same values as Linux.',
    verification: 'Golden log plus a diff against mainline.',
  },
  {
    id: 'M4b',
    name: 'A real userland',
    delivers: 'A standard library program running unmodified.',
    state: 'done',
    exit: 'A standard library program runs unmodified, prints and exits 0.',
    verification: 'Golden log.',
  },
  {
    id: 'M4c',
    name: 'Demand paging',
    delivers: 'Demand paging for anonymous mappings.',
    state: 'done',
    exit: 'An anonymous mapping faults its pages in on demand, and the per page tail measures a real fault.',
    verification: 'The weights workload against mainline.',
  },
  {
    id: 'E1',
    name: 'Initramfs',
    delivers: 'The initramfs as a read only file system.',
    state: 'done',
    exit: 'A user program reads a file from it.',
    verification: 'openat, read, lseek, fstat.',
  },
  {
    id: 'E2',
    name: 'File mapping and large pages',
    delivers: 'A file backed mapping and large pages.',
    state: 'done',
    exit: 'The weights are mapped with 2 MiB pages.',
    verification: 'The weights workload, before and after.',
  },
  {
    id: 'E2b',
    name: 'Resident weights',
    delivers: 'Large pages for file mappings and mlock residency of the weights.',
    state: 'done',
    exit: 'The weights are resident: mlock keeps them in memory and the pages are large.',
    verification: 'The "locked pages" line of the boot log.',
  },
  {
    id: 'E3a',
    name: 'Priorities',
    delivers: 'A scheduler with priorities and wakeup preemption.',
    state: 'done',
    exit: 'A scheduler with priorities and wakeup preemption.',
    verification: 'The threads workload and the wakeup test.',
  },
  {
    id: 'E3b',
    name: 'A short tail',
    delivers: 'The tail of the scheduler measured, and the policy that shortens it.',
    state: 'done',
    exit: 'The tail of a runnable thread stays inside a budget.',
    verification: 'The threads workload, published with its p99.',
  },
  {
    id: 'E4',
    name: 'The inference runtime',
    delivers: 'A real model, token for token against the reference.',
    state: 'done',
    exit: 'A Rust inference runtime generates the same tokens as the reference implementation.',
    verification: 'Token for token diff against the reference.',
  },
  {
    id: 'E5',
    name: 'The runtime measured',
    delivers: 'Tokens per second and time to first token, both kernels, same machine.',
    state: 'done',
    exit: 'Tokens per second and time to first token, both kernels, same machine.',
    verification: 'The rig, with the raw data published.',
  },
  {
    id: 'E6',
    name: 'The agent',
    delivers: 'Model, tool call, tool result and model, with a grammar that forces the call.',
    state: 'done',
    exit: 'The agent asks the model for a tool call, runs the tool and goes on, and the call is always well formed.',
    verification: 'The agent gate in CI, and the grammar tests.',
  },
  {
    id: 'E7',
    name: 'The agent measured',
    delivers: 'The latency of a step and the tokens of a step, on both kernels.',
    state: 'done',
    exit: 'The agent is measured on both kernels: the latency of a step and the tokens of a step.',
    verification: 'The rig, with the raw data published.',
  },
  {
    id: 'M8',
    name: 'The benchmark rig',
    delivers: 'Seventeen measurements, the comparison published with its raw data.',
    state: 'done',
    exit: 'Published benchmark report: raw CSV, median, p99, sigma, N >= 30.',
    verification: 'The bench scripts.',
  },
  {
    id: 'M9',
    name: 'SMP',
    delivers: 'ACPI MADT, INIT-SIPI-SIPI, LAPIC and IOAPIC, IPIs, four processors.',
    state: 'done',
    exit: 'Boots with 4 vCPUs; every processor runs its own threads and reports a heartbeat.',
    verification: 'Boot gate with four processors in CI.',
  },
  {
    id: 'M10',
    name: 'The network',
    delivers: 'The PCI bus, a virtio device, IPv4 and UDP, the sockets of the ABI.',
    state: 'done',
    exit: 'The kernel reaches the network: an agent calls an API and reads the answer.',
    verification: 'The networking gate in CI.',
  },
  {
    id: 'M12',
    name: 'TCP',
    delivers: 'TCP in the kernel, so the agent can speak HTTP.',
    state: 'done',
    exit: 'A request and a reply over TCP.',
    verification: 'The networking gate in CI, with a request and a reply over TCP.',
  },
  {
    id: 'E8',
    name: 'The tools of an agent',
    delivers: 'The agent writes a file and lists a directory: the tools need a file system the kernel serves.',
    state: 'next',
    exit: 'The tools need a file system the kernel serves.',
    verification: 'The tools against the same program on both kernels.',
  },
  {
    id: 'M11',
    name: 'A quantized checkpoint',
    delivers: 'A quantized model fits in the memory of the machine and keeps its answer.',
    state: 'next',
    exit: 'A quantized model fits in the memory of the machine and keeps its answer.',
    verification: 'The runtime against the reference, with the digest of the checkpoint.',
  },
  {
    id: 'M13',
    name: 'TLS',
    delivers: 'TLS in the kernel, so the agent can read the certificate of an API.',
    state: 'next',
    exit: 'Against a server with a certificate.',
    verification: 'The networking gate in CI.',
  },
  {
    id: 'M5',
    name: 'A shell',
    delivers: 'A /init busybox shell runs "ls | wc -l" with the same output as Linux.',
    state: 'deferred',
    exit: 'The same output as Linux.',
    verification: 'Diff suite.',
  },
  {
    id: 'M6',
    name: 'Dynamic linking',
    delivers: 'A musl dynamic binary runs from a read-only ext2 rootfs.',
    state: 'deferred',
    exit: 'A musl dynamic binary runs from a read-only ext2 rootfs.',
    verification: 'Golden log.',
  },
  {
    id: 'M7',
    name: 'Signals and pipelines',
    delivers: 'A signal and pipeline test suite produces identical output to Linux.',
    state: 'deferred',
    exit: 'Identical output to Linux.',
    verification: 'Diff suite in CI.',
  },
];

export type NextStep = { title: string; why: string; id?: string };

export const nextSteps: NextStep[] = [
  {
    id: 'M13',
    title: 'TLS',
    why: 'Almost every API people call today is HTTPS, and an agent that cannot read a certificate cannot use them.',
  },
  {
    id: 'E8',
    title: 'The tools an agent of today expects',
    why: 'Files of the machine and commands: the agent of today reads the files of the person and runs things.',
  },
  {
    title: 'The model context protocol',
    why: 'The way the clients of today talk to a model and to its tools. An agent that speaks it works with the clients people already have.',
  },
  {
    id: 'M11',
    title: 'A checkpoint that closes the conversation',
    why: 'The model of 135 million parameters reads a file and then asks for it again. The grammar is not the problem: the checkpoint is.',
  },
  {
    title: 'A host with hardware acceleration',
    why: 'The only thing missing for the timings to describe a machine instead of an emulator.',
  },
  {
    title: 'A scheduler with a better tail',
    why: 'Batching inference requests lives on the tail of the scheduler.',
  },
];

export const deferred = [
  'Processes and a real file system',
  'Dynamic linking',
  'Signals',
  'A shell test suite',
];

export type Benchmark = {
  metric: string;
  madrust: string;
  mainline: string;
  mainlineNoMitigations: string;
  group: 'operation' | 'inference' | 'agent';
};

export const benchmarks: Benchmark[] = [
  {
    group: 'operation',
    metric: 'System call',
    madrust: '376 ns',
    mainline: '29.3 us',
    mainlineNoMitigations: '1.04 us',
  },
  {
    group: 'operation',
    metric: 'Map and touch memory',
    madrust: '6.83 ms/MiB',
    mainline: '25.8 ms/MiB',
    mainlineNoMitigations: '8.69 ms/MiB',
  },
  {
    group: 'operation',
    metric: 'Fault in memory, 4 KiB pages',
    madrust: '2.01 ms/MiB',
    mainline: '44.6 ms/MiB',
    mainlineNoMitigations: '13.7 ms/MiB',
  },
  {
    group: 'operation',
    metric: 'Fault in memory, 2 MiB pages',
    madrust: '825 us/MiB',
    mainline: '44.9 ms/MiB',
    mainlineNoMitigations: '13.0 ms/MiB',
  },
  {
    group: 'operation',
    metric: 'Futex round trip',
    madrust: '6.53 us',
    mainline: '407 us',
    mainlineNoMitigations: '97.7 us',
  },
  {
    group: 'operation',
    metric: 'Four threads, spawn to join',
    madrust: '620 us',
    mainline: '7.73 ms',
    mainlineNoMitigations: '4.59 ms',
  },
  {
    group: 'operation',
    metric: 'Boot to the first result',
    madrust: '12.9 s',
    mainline: '206 s',
    mainlineNoMitigations: '63.8 s',
  },
  {
    group: 'operation',
    metric: 'Resident memory of the guest',
    madrust: '319 MB',
    mainline: '476 MB',
    mainlineNoMitigations: '483 MB',
  },
  {
    group: 'inference',
    metric: 'Time to the first token',
    madrust: '11.25 s',
    mainline: '24.67 s',
    mainlineNoMitigations: '22.82 s',
  },
  {
    group: 'inference',
    metric: 'Tokens per second',
    madrust: '0.45 tok/s',
    mainline: '0.20 tok/s',
    mainlineNoMitigations: '0.21 tok/s',
  },
  {
    group: 'agent',
    metric: 'Seconds of an agent loop',
    madrust: '821 s',
    mainline: '2793 s',
    mainlineNoMitigations: '2570 s',
  },
  {
    group: 'agent',
    metric: 'Resident memory with the model',
    madrust: '857 MB',
    mainline: '1021 MB',
    mainlineNoMitigations: '1023 MB',
  },
];

export const benchmarkMethod = {
  headline: 'One processor. 512 MiB for the operations, 1024 MiB for the model and the agent.',
  detail:
    'QEMU 8.2.2, Linux 7.3.0 built from the mainline tree, the same initramfs and the same binary on both sides.',
  datasets: [
    { label: 'The operations', path: 'bench/results/vps-1cpu/' },
    { label: 'The model', path: 'bench/results/inference-native/' },
    { label: 'The agent', path: 'bench/results/agent/' },
  ],
};

export const benchmarkCaveats = [
  {
    title: 'The ratios are the claim',
    body: 'Between 1.3 and 7.4 times cheaper per operation, three times faster in the agent loop, two thirds of the memory of the guest.',
  },
  {
    title: 'The absolute numbers describe an emulator',
    body: 'There is no hardware acceleration on the host, so a wall clock statement about a processor is not supported yet. A host with KVM is the open issue for that.',
  },
  {
    title: 'Every loss is published',
    body: 'The reference kernel without mitigations is measured too, and the batches where a boot did not finish are in the repository with the samples that are missing.',
  },
];

export const claims = [
  'Same userspace, same QEMU version, same machine model, same rootfs, same benchmark scripts, raw results committed to the repository.',
  'Two areas where a purpose-built kernel can plausibly win in a private VM: syscall entry cost and fork/exec plus tail latency.',
  'Every loss is published too. No cherry-picking, no "mitigations off" as the only comparison point.',
];

export const faq: { question: string; answer: string }[] = [
  {
    question: 'Is this Linux?',
    answer:
      'No. MadRust is an x86_64 kernel written from scratch that is compatible with the Linux userspace ABI: the same system calls, the same struct layouts, the same ELF loading rules. It is not a distribution and it does not replace Linux.',
  },
  {
    question: 'Does it run on my machine?',
    answer:
      'It runs in QEMU on x86_64, with -M q35. It is not bare metal yet, and without KVM or HVF the timings describe an emulator. Hardware acceleration is the open issue.',
  },
  {
    question: 'What does it need?',
    answer:
      'QEMU 8.2 or newer, 512 MiB of memory for the operations or 1024 MiB for the model and the agent, and a virtio network device for the network. The full comparison against mainline Linux needs a Linux host.',
  },
  {
    question: 'Which model does it serve?',
    answer:
      'SmolLM2-135M-Instruct, in bfloat16, with the revision pinned and the SHA-256 verified. A quantized checkpoint is produced by the same repository and measured too: Q8 reproduces the reference tokens, Q4 changes the answer, and that change is published as a loss.',
  },
  {
    question: 'Does the agent call cloud APIs?',
    answer:
      'The model is local, inside the initramfs. The agent can also reach the network: a tool speaks HTTP to an API and another sends a datagram. It is an agent with a local brain and hands that reach the world.',
  },
  {
    question: 'Is it safe to run?',
    answer:
      'It is not production ready, and it is not hardened on purpose: no KPTI, no retpoline, no SMAP enforcement. The threat model is a private single tenant VM, and the limitation is documented rather than quietly dropped.',
  },
  {
    question: 'What licence is it under?',
    answer:
      'GPL-2.0-only. Kernel crates may only use MIT, BSD or ISC licensed code, because GPL-2.0-only is incompatible with Apache-2.0-only.',
  },
  {
    question: 'Why should the benchmarks be believed?',
    answer:
      'Same host, same QEMU version, same machine model, same rootfs, same binary and same scripts. The raw CSV is committed, the losses are included, and the kernel with mitigations off is measured too, so the comparison cannot be cherry-picked.',
  },
  {
    question: 'How much of it was written by an AI agent?',
    answer:
      'All of it, and that is stated on purpose. The design decisions, the milestone criteria and the verification belong to a human; every milestone has an objective exit criterion that a machine checks in CI. The repository is the evidence.',
  },
];

export const build = {
  requirements: [
    { name: 'Rust nightly', detail: 'nightly-2026-10-24, pinned in rust-toolchain.toml.' },
    { name: 'Components', detail: 'rust-src, llvm-tools, rustfmt, clippy.' },
    { name: 'Target', detail: 'x86_64-unknown-linux-musl for the userland, installed by make musl-std.' },
    { name: 'QEMU', detail: 'qemu-system-x86_64, 8.2 or newer.' },
    { name: 'Disk images', detail: 'xorriso and mtools, only if you build disk images.' },
  ],
  commands: [
    { cmd: 'make build', what: 'Build the kernel.' },
    { cmd: 'make run', what: 'Boot it in QEMU, serial on stdout.' },
    { cmd: 'make test', what: 'Host unit tests of every crate.' },
    { cmd: 'make fmt-check', what: 'The format of every crate.' },
    { cmd: 'make clippy', what: 'The clippy of every crate, for its target.' },
    { cmd: 'make clippy-host', what: 'The clippy of every crate, for the host.' },
    { cmd: 'make agent agent-initramfs', what: 'Build the agent and its initramfs.' },
    { cmd: 'make print-kernel-path', what: 'Print the path of the built kernel.' },
  ],
  qemu: `qemu-system-x86_64 -M q35 -m 512M \\
  -kernel "$(make -s print-kernel-path)" \\
  -display none -monitor none -no-reboot -serial stdio \\
  -device isa-debug-exit,iobase=0xf4,iosize=0x04`,
  qemuNote: 'The boot ends with exit status 33, which means boot OK.',
  debug: `qemu-system-x86_64 -M q35 -m 512M \\
  -kernel "$(make -s print-kernel-path)" -s -S \\
  -display none -monitor none -no-reboot -serial stdio`,
  debugNote:
    'gdbstub on port 1234, CPU halted. Attach with lldb "$(make -s print-kernel-path)" and gdb-remote 1234 on macOS, or gdb and target remote :1234 on Linux.',
  install: `rustup toolchain install nightly-2026-10-24 \\
  --component rust-src --component llvm-tools --component rustfmt --component clippy`,
};

export const repository = [
  { crate: 'madrust-arch', what: 'x86_64 hardware access, port I/O, boot trampoline' },
  { crate: 'madrust-bootinfo', what: 'pure parsing of the PVH boot handover structures' },
  { crate: 'madrust-drivers', what: '16550A UART, isa-debug-exit' },
  { crate: 'madrust-mm', what: 'paging, the buddy allocator, the heap, the mappings' },
  { crate: 'madrust-sched', what: 'the scheduler and its policy' },
  { crate: 'madrust-elf', what: 'the ELF64 reader' },
  { crate: 'madrust-initramfs', what: 'the reader of the cpio archive of the initramfs' },
  { crate: 'madrust-kernel', what: 'the no_std kernel binary' },
  { crate: 'madrust-userland', what: 'the test program that runs unmodified in ring 3' },
  { crate: 'madrust-bench', what: 'the workload that measures the kernel' },
  { crate: 'madrust-infer', what: 'the inference runtime, the grammar, the merge encoder' },
  { crate: 'madrust-agent', what: 'the agent loop: model, tool call, tool result' },
  { crate: 'madrust-net', what: 'the protocols of the network path' },
  { crate: 'madrust-netclient', what: 'the client of the network' },
  { crate: 'madrust-netstream', what: 'the stream of the network' },
  { crate: 'madrust-http', what: 'the HTTP of the agent' },
  { crate: 'bench', what: 'the rig, the workloads, the reference build, the datasets' },
  { crate: 'docs', what: 'architecture, boot protocol, benchmarking, VM setup' },
  { crate: 'scripts', what: 'the generator of the interrupt stubs' },
];

export const docs = [
  {
    slug: 'architecture',
    title: 'Architecture',
    summary: 'The crates, the design decisions, the milestone ladder, the risks and the stop rules.',
    file: 'docs/ARCHITECTURE.md',
  },
  {
    slug: 'boot',
    title: 'Boot protocol',
    summary: 'What the Xen PVH handover guarantees, the trampoline, the memory map and how to debug it.',
    file: 'docs/BOOT.md',
  },
  {
    slug: 'benchmarking',
    title: 'Benchmarking',
    summary: 'The method, the rig, the protocol and the raw numbers with their caveats.',
    file: 'docs/BENCHMARKING.md',
  },
  {
    slug: 'quantization',
    title: 'The quantized checkpoint',
    summary: 'The format of the quantized checkpoint, how to produce it and how to check it.',
    file: 'docs/QUANTIZATION.md',
  },
  {
    slug: 'learning',
    title: 'Learning path',
    summary: 'How to read the kernel and the userland, the experiments to run and the sources worth reading next.',
    file: 'docs/LEARNING.md',
  },
  {
    slug: 'vm-setup',
    title: 'The Linux build host',
    summary: 'How to build mainline Linux and a rootfs, and the artifact handoff for the comparison.',
    file: 'docs/VM-SETUP.md',
  },
  {
    slug: 'report',
    title: 'The report',
    summary: 'The measurements, the findings, the losses and the threats to validity.',
    file: 'REPORT.md',
  },
];

export const risks = [
  {
    title: 'Scope creep towards a small Linux',
    mitigation: 'The cut list is frozen: no kernel modules, no namespaces, no cgroups.',
  },
  {
    title: 'The dynamic loader is the hardest single item',
    mitigation: 'Static musl binaries first, dynamic musl later.',
  },
  {
    title: 'ABI divergence in errno values or structure layouts',
    mitigation: 'Diff the same binary behaviour on Linux and MadRust from M4 onwards.',
  },
  {
    title: 'Concurrency bugs once SMP arrives',
    mitigation: 'Lockdep-lite from M3, one CPU until the rig lands.',
  },
  {
    title: 'Burnout',
    mitigation: 'Visible weekly exit criteria and explicit stop rules.',
  },
  {
    title: 'QEMU is not hardware',
    mitigation: 'Bare metal is an explicit non-goal; all results are labelled as QEMU results.',
  },
];

export const stopRules = [
  'Network stack, not started.',
  'SMP: publish single CPU benchmarks instead.',
  'Dynamic linking: static musl only.',
  'Signals: keep kill, exit and SIGCHLD only.',
  'Benchmark suite: keep only the getpid and context switch benchmarks.',
];

export const estimates = {
  effort: 'About 34 weeks in total for a solo developer with steady spare time, roughly 12 hours per week.',
};
