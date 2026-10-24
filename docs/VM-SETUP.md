# The Linux build host

MadRust builds and boots on the Mac, but some work needs a real Linux host:
building mainline Linux, building a distribution rootfs, and running the
benchmark rig.

Two options work. Pick one.

## Option A: a local virtual machine

Install UTM (Apple Virtualization based, runs on Apple Silicon):

```sh
brew install --cask utm
```

Download the Ubuntu 24.04 LTS arm64 desktop ISO from
`https://releases.ubuntu.com/24.04/` and create a new virtual machine in UTM:

- guest OS: Linux, using Apple Virtualization,
- ISO: the downloaded image,
- memory: 8 GB, CPU cores: 4, disk: 64 GB.

Boot it, install Ubuntu, and set a short hostname and user name. Disable disk
encryption to keep the setup simple. Enable SSH inside the guest if you want
to drive it from the host.

Sharing a host directory with the guest is optional: UTM can expose one over
virtiofs, but check the mount point inside the guest (commonly under
`/Volumes` or `/mnt`) instead of assuming a path. Cloning the repository over
SSH from the host works just as well.

Inside the guest:

```sh
sudo apt update
sudo apt install -y \
    build-essential git curl ca-certificates \
    qemu-system-x86 xorriso mtools \
    llvm gdb cpio rsync python3 bc flex bison libssl-dev libelf-dev
```

`qemu-system-x86` provides `qemu-system-x86_64`. `gdb` is the debugger to use
when the kernel is built and run inside the VM.

## Option B: an external x86_64 Linux host

A remote machine works as well, and it is the better choice for the benchmark
rig because the host is real x86_64 hardware. The workflow is then a handoff:
the kernel project stays here, and the artifacts built on the remote host are
brought over.

## What to build on the Linux host

### Mainline Linux

```sh
git clone --depth=1 https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
cd linux
make defconfig
scripts/config --enable CONFIG_PVH=y
make -j"$(nproc)"
```

The uncompressed `vmlinux` carries the PVH note and boots under QEMU with the
same `-kernel` flag used for MadRust, which is what the benchmark rig
requires. Keep the `vmlinux` file and the kernel version string.

### A rootfs

```sh
sudo apt install -y debootstrap
debootstrap --arch=amd64 bookworm /srv/rootfs http://deb.debian.org/debian
cd /srv/rootfs && find . | cpio -o -H newc | gzip > /srv/initramfs.cpio.gz
```

The initramfs is passed to both kernels with `-initrd`, so both run the exact
same userland.

## The handoff

These are the artifacts the benchmark work needs. Nothing else has to leave
the Linux host:

| Artifact | What it is |
|---|---|
| `vmlinux` | Mainline kernel, built with `CONFIG_PVH=y` |
| `initramfs.cpio.gz` | The rootfs both kernels boot |
| `kernel-version.txt` | `make kernelversion` output, for the report |
| `host-info.txt` | `uname -a`, `nproc`, `free -h`, `qemu-system-x86_64 --version`, and whether `/dev/kvm` exists |

## Building and running MadRust from the Linux host

The repository pins the toolchain in `rust-toolchain.toml`, so `cargo` inside
the checkout installs the right nightly automatically on first use, with the
`rust-src` and `llvm-tools` components it lists.

```sh
git clone https://github.com/NicolasBecasAzagra/MadRust.git
cd MadRust
cargo build --release
qemu-system-x86_64 -M q35 -m 512M \
    -kernel target/x86_64-madrust/release/madrust-kernel \
    -display none -monitor none -no-reboot -serial stdio \
    -device isa-debug-exit,iobase=0xf4,iosize=0x04
```

Running an x86_64 guest inside an arm64 guest uses emulation in emulation and
is slow. For interactive work prefer building on the Mac host and running
QEMU there; use the Linux host for mainline Linux, rootfs images and the
benchmark rig.

## Hardware acceleration

The benchmark rig wants `/dev/kvm`. A host without it runs QEMU in TCG mode,
where every guest instruction is translated by the emulator. That has two
consequences:

- the absolute numbers describe the emulator, not the processor;
- a system call costs a trap into the emulator, which can dominate the
  difference between two kernels and hide the effect being measured.

Runs without KVM are still valid as a relative comparison, because both
kernels run under identical emulation, but they cannot support a claim about
hardware performance. The report has to state which mode produced the data.
