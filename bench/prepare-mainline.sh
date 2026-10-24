#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Build the mainline Linux image used as the reference in the benchmark rig.
#
# The image has to carry the Xen PVH note, because QEMU loads a 64-bit ELF
# kernel through that path: without the note, QEMU refuses the image with
# "Error loading uncompressed kernel without PVH ELF Note". With the note,
# mainline and MadRust are started by the same handover, with the same
# command line, which is what makes the comparison fair.
#
# Usage: bench/prepare-mainline.sh [OUTPUT_DIRECTORY]
#   OUTPUT_DIRECTORY  where to put vmlinux and kernel-version.txt,
#                     default artifacts/
#
# The script needs a Linux host with build-essential, flex, bison, libssl-dev,
# libelf-dev and bc. It does not need a kernel from any distribution: the
# defconfig of the mainline tree is enough.

set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$repo/artifacts"}
mkdir -p "$output"

if [ ! -d "$repo/artifacts/linux" ]; then
    echo "prepare-mainline: cloning the mainline tree"
    git clone --depth=1 https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git \
        "$repo/artifacts/linux"
fi

cd "$repo/artifacts/linux"
make defconfig
scripts/config --enable CONFIG_PVH=y
# The workload asks for large pages with madvise. A reference kernel without
# transparent huge pages refuses the advice with EINVAL, and then the large
# page regime cannot be compared at all.
scripts/config --enable CONFIG_TRANSPARENT_HUGEPAGE
scripts/config --disable CONFIG_TRANSPARENT_HUGEPAGE_DEFAULT_ON
make -j"$(nproc 2>/dev/null || echo 4)"

cp vmlinux "$output/vmlinux"
make kernelversion > "$output/kernel-version.txt"

echo "prepare-mainline: wrote $output/vmlinux and $output/kernel-version.txt"
echo "prepare-mainline: kernel $(cat "$output/kernel-version.txt")"
