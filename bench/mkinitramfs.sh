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
# EXTRA_DIR, when set, adds every regular file in that directory to the archive
# root. The inference runtime uses it for /model.safetensors, /vocab.json and
# /config.json, and the rig uses it for the /net.conf of the network client.
# The names `bench` and `init` are reserved for the workload.
#
# The script builds the binary with `make bench`, or with `make netclient` when
# the binary is the network client, if it is missing. `cpio` and `find` are the
# only external tools; the archive is not compressed, so both kernels read
# exactly the same bytes.

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
    # The script can pack the benchmark workload or the network client, and
    # each of them has its own target. The name of the binary is what decides,
    # so a caller that asks for a program that is not built gets the program
    # built and not a different one.
    target=bench
    case ${binary##*/} in
        madrust-netclient.elf) target=netclient ;;
    esac
    make -C "$repo" --no-print-directory "$target"
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

if [ -n "${EXTRA_DIR:-}" ]; then
    if [ ! -d "$EXTRA_DIR" ]; then
        echo "mkinitramfs: EXTRA_DIR is not a directory: $EXTRA_DIR" >&2
        exit 1
    fi
    for extra in "$EXTRA_DIR"/*; do
        [ -f "$extra" ] || continue
        name=${extra##*/}
        case $name in
            bench|init)
                echo "mkinitramfs: EXTRA_DIR must not provide $name" >&2
                exit 1
                ;;
        esac
        cp "$extra" "$work/$name"
    done
fi

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

# The name of an entry, without the leading "./" that some cpio builds keep
# and others drop. The archive is read with a plain loop below, and the
# names are compared against what the archive should hold, so the difference
# between the two spellings must not matter.
def normalised(name):
    while name.startswith(b"./"):
        name = name[2:]
    return name

# Rewrite the archive so the data of every regular file starts on a page
# boundary. MadRust maps a file without copying it, so the page table entry has
# to point at the page that already holds the bytes; a `newc` archive only
# aligns file data to four bytes. A hidden regular file with enough zero bytes
# shifts the next header, so the padding costs a few KiB and nothing has to
# change in either kernel.
path = sys.argv[1]
data = bytearray(open(path, "rb").read())
entries = []
found_console = False
offset = 0
while offset + 110 <= len(data):
    magic = bytes(data[offset : offset + 6])
    if magic not in (b"070701", b"070702"):
        raise SystemExit("mkinitramfs: bad cpio magic")
    fields = [
        int(data[offset + 6 + 8 * i : offset + 6 + 8 * (i + 1)], 16)
        for i in range(13)
    ]
    name_size = fields[11]
    file_size = fields[6]
    name_start = offset + 110
    name = bytes(data[name_start : name_start + name_size - 1])
    data_start = (name_start + name_size + 3) & ~3
    body = bytes(data[data_start : data_start + file_size])
    plain = normalised(name)
    if plain == b"dev/console":
        fields[1] = 0o20600  # S_IFCHR | 0600
        fields[9] = 5  # major
        fields[10] = 1  # minor
        found_console = True
    elif plain == b"dev/null":
        fields[1] = 0o20666  # S_IFCHR | 0666
        fields[9] = 1  # major
        fields[10] = 3  # minor
    entries.append((magic, fields, name, body))
    if name == b"TRAILER!!!":
        break
    offset = (data_start + file_size + 3) & ~3
else:
    raise SystemExit("mkinitramfs: no cpio trailer")

if not found_console:
    # The console node is the only way the program of the guest reaches the
    # serial port. Without it the output of the run goes to a file that
    # nobody reads, and the run looks like a guest that says nothing.
    raise SystemExit("mkinitramfs: the archive has no /dev/console entry")


def align4(value):
    return (value + 3) & ~3


def encode(magic, fields, name, size):
    fields = fields.copy()
    fields[6] = size
    fields[11] = len(name) + 1
    block = magic + b"".join(f"{value:08x}".encode() for value in fields)
    block += name + b"\0"
    block += b"\0" * (-len(block) % 4)
    return block


out = bytearray()
pad_index = 0
for magic, fields, name, body in entries:
    regular = (fields[1] & 0o170000) == 0o100000
    if regular and body and name != b"TRAILER!!!":
        # Compute the data offset the target would get, then pick the padding
        # length that moves the next header to the page-aligned residue.
        pad_name = b"./.align%d" % pad_index
        base = align4(len(out) + 110 + len(pad_name) + 1)
        header = 110 + len(name) + 1
        aligned = header + (-header % 4)
        want = (-aligned) % 4096
        pad_size = (want - base) % 4096
        pad_fields = [100000 + pad_index, 0o100644, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]
        out += encode(b"070701", pad_fields, pad_name, pad_size)
        out += b"\0" * pad_size
        out += b"\0" * (-len(out) % 4)
        pad_index += 1
    out += encode(magic, fields, name, len(body))
    out += body
    out += b"\0" * (-len(out) % 4)

open(path, "wb").write(out)
PY

echo "mkinitramfs: wrote $output ($(wc -c < "$output" | tr -d ' ') bytes)"
