# Linux VM setup

The Mac host is enough to build and boot MadRust: the kernel cross-compiles
with `rustc` and `rust-lld`, and QEMU runs the x86_64 image under emulation.

A Linux virtual machine is needed later, for work that cannot be done from
macOS:

- building mainline Linux, to diff the ABI and to run the benchmark rig,
- building musl and busybox test binaries,
- building a Debian or Ubuntu rootfs image.

## 1. Create the VM

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

## 2. Install the build dependencies

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

## 3. Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
. "$HOME/.cargo/env"
```

The repository pins the toolchain in `rust-toolchain.toml`, so `cargo` inside
the checkout installs the right nightly automatically on first use, with the
`rust-src` and `llvm-tools` components it lists.

## 4. Build and boot

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
QEMU there; use the VM for mainline Linux, rootfs images and the benchmark
rig, where the guest is booted with hardware acceleration or on a dedicated
x86_64 host.

## 5. Mainline Linux (later)

```sh
git clone --depth=1 https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
cd linux
make defconfig
scripts/config --enable CONFIG_PVH=y
make -j"$(nproc)"
```

The uncompressed `vmlinux` carries the PVH note and boots under QEMU with the
same `-kernel` flag used for MadRust, which is what the benchmark rig
requires.

## 6. Rootfs images (later)

```sh
sudo apt install -y debootstrap
debootstrap --arch=amd64 bookworm /srv/rootfs http://deb.debian.org/debian
cd /srv/rootfs && find . | cpio -o -H newc | gzip > /srv/initramfs.cpio.gz
```

The initramfs is passed to both kernels with `-initrd`, so both run the exact
same userland.
