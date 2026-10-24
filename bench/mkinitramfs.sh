#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only
#
# Build the benchmark initramfs: one uncompressed cpio `newc` archive with the
# workload binary at /bench and at /init.
#
# MadRust prefers /bench; mainline Linux runs /init as PID 1. Both names carry
# the same bytes, so the archive works on either kernel without a flag.
#
# Usage: bench/mkinitramfs.sh [OUTPUT] [BINARY]
#   OUTPUT  archive to write, default bench/initramfs.cpio
#   BINARY  workload to pack, default madrust-bench/madrust-bench.elf
#
# The script builds the binary with `make bench` when it is missing. `cpio` and
# `find` are the only external tools; the archive is not compressed, so both
# kernels read exactly the same bytes.

set -eu

repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
output=${1:-"$repo/bench/initramfs.cpio"}
binary=${2:-"$repo/madrust-bench/madrust-bench.elf"}

# The archive is written from inside the temporary directory, so a relative
# output path has to be anchored before the subshell changes directory.
case $output in
    /*) ;;
    *) output="$PWD/$output" ;;
esac

if [ ! -f "$binary" ]; then
    make -C "$repo" --no-print-directory bench
fi

if [ ! -f "$binary" ]; then
    echo "mkinitramfs: workload binary not found: $binary" >&2
    exit 1
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/madrust-initramfs.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM

cp "$binary" "$work/bench"
cp "$binary" "$work/init"
chmod 755 "$work/bench" "$work/init"

# Mainline opens /dev/console before it execs /init. A distro kernel does not
# mount devtmpfs for an initramfs, so the archive has to carry the node or the
# workload would run without descriptors 0 to 2. The files below are created as
# empty regular files because a non-root archive build cannot call mknod; the
# Python step rewrites their mode and device numbers in the finished `newc`
# archive. Both are four byte aligned, so only the header changes.
mkdir "$work/dev"
: > "$work/dev/console"
: > "$work/dev/null"
chmod 755 "$work" "$work/dev"
chmod 600 "$work/dev/console"
chmod 666 "$work/dev/null"

# `find .` gives the archive relative names (./bench, ./init); both kernels
# strip the leading ./ when they resolve a path.
(cd "$work" && find . -print | LC_ALL=C cpio -o -H newc > "$output")

python3 - "$output" <<'PY'
import sys

path = sys.argv[1]
data = bytearray(open(path, "rb").read())
offset = 0
while offset + 110 <= len(data):
    if data[offset : offset + 6] not in (b"070701", b"070702"):
        raise SystemExit("mkinitramfs: bad cpio magic")
    def number(at):
        return int(data[offset + at : offset + at + 8], 16)
    name_size = number(94)
    file_size = number(54)
    name_start = offset + 110
    name = bytes(data[name_start : name_start + name_size - 1])
    if name == b"TRAILER!!!":
        break
    data_start = (name_start + name_size + 3) & ~3
    if name == b"./dev/console":
        data[offset + 14 : offset + 22] = b"00002180"  # S_IFCHR | 0600
        data[offset + 78 : offset + 86] = b"00000005"  # major
        data[offset + 86 : offset + 94] = b"00000001"  # minor
    elif name == b"./dev/null":
        data[offset + 14 : offset + 22] = b"000021b6"  # S_IFCHR | 0666
        data[offset + 78 : offset + 86] = b"00000001"  # major
        data[offset + 86 : offset + 94] = b"00000003"  # minor
    offset = (data_start + file_size + 3) & ~3
open(path, "wb").write(data)
PY

echo "mkinitramfs: wrote $output ($(wc -c < "$output" | tr -d ' ') bytes)"
