# Boot protocol

MadRust boots through the Xen PVH handover. This document records what the
bootloader guarantees, what the kernel must do, and how to debug it.

## Why PVH

QEMU's `-kernel` loader accepts three kinds of x86 images:

- Linux `bzImage` (the Linux boot protocol),
- 64-bit ELF with a PVH note,
- 32-bit Multiboot images.

Multiboot2 is **not** supported for 64-bit images. `hw/i386/multiboot.c` in
QEMU rejects them, and QEMU 11.1.2 reports:

```text
qemu-system-x86_64: Error loading uncompressed kernel without PVH ELF Note
```

when a 64-bit ELF without a PVH note is passed to `-kernel`. This was
reproduced locally on QEMU 11.1.2 with a minimal ELF64.

PVH was chosen over a Multiboot2 plus external bootloader setup because:

- QEMU implements the PVH handover natively, so no ISO has to be built and no
  bootloader binary has to be vendored;
- the same handover path boots mainline Linux (`vmlinux` carries a PVH note
  when `CONFIG_PVH` is enabled), so both kernels can be booted with the same
  QEMU command line, the same `-initrd` and the same `-append`. That is what
  makes the benchmark comparison fair.

## The ELF note

The kernel ELF must carry a note of type `XEN_ELFNOTE_PHYS32_ENTRY` (`0x12`),
name `Xen`, whose descriptor is the physical address of the 32-bit entry
point:

```text
namesz = 4, descsz = 4, type = 0x12, name = "Xen\0", desc = <u32 entry address>
```

The note is emitted by the linker script in the `.note.Xen` output section so
the descriptor always matches the real entry address. Verify it with:

```sh
llvm-readelf -n target/x86_64-madrust/release/madrust-kernel
```

## CPU state at the 32-bit entry

The loader jumps to the 32-bit entry with:

- `cr0`: PE set, every other writable bit cleared.
- `cr4`: all bits cleared, so paging is off.
- `cs`: 32-bit read/execute segment, base 0, limit `0xFFFFFFFF`.
- `ds`, `es`, `ss`: 32-bit read/write segments, base 0, limit `0xFFFFFFFF`.
- `tr`: 32-bit TSS, base 0, limit `0x67`.
- `eflags`: IF cleared, VM cleared, TF cleared.
- `ebx`: physical address of the `hvm_start_info` structure.

No stack is provided. The kernel sets up its own stack, GDT and IDT.

## `hvm_start_info`

```text
offset  size  field
0       4     magic           0x336ec578
4       4     version         1
8       4     flags
12      4     nr_modules
16      8     modlist_paddr
24      8     cmdline_paddr
32      8     rsdp_paddr
40      8     memmap_paddr
48      4     memmap_entries
52      4     reserved
```

A zero address field means the item is absent. Memory map entries are
`{ u64 addr; u64 size; u32 type; u32 reserved; }` with types `1=RAM`,
`2=RESERVED`, `3=ACPI`, `4=NVS`, `5=UNUSABLE`, `6=DISABLED`, `7=PMEM`.

QEMU fills `cmdline_paddr` from `-append` and passes `-initrd` as a module, so
the kernel receives the kernel command line and the initramfs through the same
structures mainline Linux uses.

## Trampoline flow

The 32-bit entry does, in order:

1. Point `ESP` at the stack reserved in `.bss`.
2. Save `EBX` (the `start_info` physical address) into a known `.bss` slot.
3. Build a 4-level page table in `.bss` identity-mapping the first 1 GiB with
   2 MiB pages.
4. Enable `CR4.PAE`, set `EFER.LME` (MSR `0xC0000080`, bit 8), load `CR3`.
5. Set `CR0.PG | CR0.PE` and far jump to the 64-bit block with a temporary
   GDT.
6. In the 64-bit block, load the data selectors and call the Rust entry with
   the `start_info` pointer.

M0 links the kernel identity-mapped at physical `0x100000`, so virtual and
physical addresses coincide and the note descriptor is directly the link
address. Relocation to the higher half happens in M2.

## Higher half

The kernel is linked at the higher half and loaded at a low physical address:

```text
virtual:  0xffffffff80000000   (kernel image)
physical: 0x00000000100000      (load address)
direct map: 0xffff888000000000 (physical memory, 2 MiB pages)
```

The PVH note keeps carrying the **physical** address of the 32-bit entry
(`0x100000`), not its virtual address, because that is what the loader jumps
to.

The boot has three stages:

1. **32-bit entry.** The processor is in protected mode with paging off, so
   the entry point first clears the kernel `.bss` (the ELF format does not
   require the loader to do it), then builds a temporary identity map of the
   first GiB and enables long mode.
2. **Switch routine.** Still running at its physical address, it builds the
   final page tables: the kernel image at the higher half (4 KiB pages for the
   first 2 MiB, because the image starts at 1 MiB and is therefore not 2 MiB
   aligned, and 2 MiB pages above that) and a direct map of physical memory.
   It loads the new PML4, moves the stack pointer to the higher-half copy of
   the boot stack and jumps to the higher half.
3. **Higher half entry.** The identity map is dropped from `PML4[0]`, so from
   that point on physical memory is reachable only through the direct map.

Page table indices: the kernel image uses PML4 entry 511, PDPT entry 510 and
PD entry 0; the direct map uses PML4 entry 273 and PDPT entry 0.

The linker script asserts that the image still fits in the 2 MiB that is
mapped with 4 KiB pages, and that the entry point is the first byte of the
image.

## Runtime sequence

The boot path is deterministic and asserted by CI:

```text
MadRust: x86_64 kernel, M3
[mm] kernel virtual: 0xffffffff80000000
[mm] direct map: 0xffff888000000000
MM DIRECTMAP OK
[boot] hvm_start_info at physical 0x0000000000001580: version=1 flags=0x0 modules=0
[mem] 9 memory map entries
[mem] RAM 0x000000000000 - 0x0000000009fc00 (639 KiB)
[mem] RAM 0x00000000100000 - 0x00001ffdf000 (523132 KiB)
[mem] total usable RAM: 536341504 bytes (511 MiB)
MEM managed frames: 131039
MEM free frames: 130679
MEM OK
HEAP OK 10000
[cpu] vendor: AuthenticAMD
[arch] GDT and TSS loaded
[arch] IDT loaded
[irq] PIC remapped, IRQ0 unmasked
[irq] PIT programmed at 100 Hz
T1 0
T2 0
T3 0
... the three threads take turns, one line per time slice ...
SCHED OK
EXCEPTION 0 DIV0 RIP=0x...
MadRust: exception survived
TICKS 100
MadRust: boot OK
```

The `DIV0` line comes from a deliberate fault injection that proves the
exception path saves the full register context, reports the faulting RIP and
returns to the interrupted code. QEMU then exits with status 33 through the
`isa-debug-exit` device.

## Debugging

```sh
qemu-system-x86_64 -M q35 -m 512M -kernel target/x86_64-madrust/release/madrust-kernel \
    -display none -monitor none -no-reboot -serial stdio -s -S
```

The CPU stays stopped until a debugger attaches on port 1234.

```sh
# macOS
lldb target/x86_64-madrust/release/madrust-kernel
# (lldb) gdb-remote 1234
# (lldb) breakpoint set -n _start

# Linux
gdb target/x86_64-madrust/release/madrust-kernel -ex 'target remote :1234'
```

`isa-debug-exit` gives the boot test a deterministic exit code: the device
`-device isa-debug-exit,iobase=0xf4,iosize=0x04` terminates QEMU with status
`(value << 1) | 1`, so writing `0x10` yields status 33.

## Later

The Linux `bzImage` boot protocol is not implemented. It is only needed to
boot on real hardware or through a boot manager; under QEMU the PVH handover
covers both MadRust and mainline Linux with identical flags.
