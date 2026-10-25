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
# The Xen guest support and its PVH part, because the option that decides
# whether the note is generated depends on them. Enabling only the general
# one is not enough: syncconfig turns it off again when the dependency is
# not set.
scripts/config --enable CONFIG_XEN=y
scripts/config --enable CONFIG_XEN_PVH=y
scripts/config --enable CONFIG_PVH=y
# The workload asks for large pages with madvise. A reference kernel without
# transparent huge pages refuses the advice with EINVAL, and then the large
# page regime cannot be compared at all.
scripts/config --enable CONFIG_TRANSPARENT_HUGEPAGE
scripts/config --disable CONFIG_TRANSPARENT_HUGEPAGE_DEFAULT_ON

# The configuration has to be resolved without questions: a new symbol in the
# tree would otherwise stop the build waiting for an answer that never comes.
# olddefconfig takes the defaults for those new symbols without asking, so the
# build is not interactive.
make olddefconfig
if ! grep -q "^CONFIG_PVH=y" .config; then
    echo "prepare-mainline: WARNING: CONFIG_PVH is not set after the configuration" >&2
    echo "prepare-mainline: the image will not boot under QEMU" >&2
fi
make -j"$(nproc 2>/dev/null || echo 4)"

cp vmlinux "$output/vmlinux"
make kernelversion > "$output/kernel-version.txt"

# The image has to carry the Xen PVH note, because QEMU loads a 64-bit ELF
# kernel through that path. Without the note it refuses the image with
# "Error loading uncompressed kernel without PVH ELF Note", which is a long
# build to find out.
# The configuration is the source of truth: the note is generated from it.
if grep -q "^CONFIG_PVH=y" .config 2>/dev/null; then
    echo "prepare-mainline: the configuration has CONFIG_PVH=y"
else
    echo "prepare-mainline: WARNING: CONFIG_PVH is not set in the configuration" >&2
    echo "prepare-mainline: QEMU will refuse to boot the image without the note" >&2
fi

# And the note itself, read with whatever the host has. The formats of the
# tools differ, so the owner and the type are matched separately and the
# leading zeros are optional.
note_found=0
for reader in llvm-readelf readelf llvm-readobj; do
    command -v "$reader" >/dev/null 2>&1 || continue
    notes="$("$reader" -n "$output/vmlinux" 2>/dev/null)"
    if printf '%s' "$notes" | grep -q "Xen" && printf '%s' "$notes" | grep -Eq "0x0*12"; then
        note_found=1
        echo "prepare-mainline: the PVH note is present in the image"
    else
        echo "prepare-mainline: the note was not found with $reader" >&2
    fi
    break
done
if [ "$note_found" -eq 0 ]; then
    echo "prepare-mainline: WARNING: the PVH note was not confirmed in the image" >&2
    echo "prepare-mainline: QEMU will refuse to boot it" >&2
    echo "prepare-mainline: check that CONFIG_PVH is enabled and that the build" >&2
    echo "prepare-mainline: used that configuration" >&2
fi

echo "prepare-mainline: wrote $output/vmlinux and $output/kernel-version.txt"
echo "prepare-mainline: kernel $(cat "$output/kernel-version.txt")"
echo "prepare-mainline: now run the rig with --mainline $output/vmlinux"
