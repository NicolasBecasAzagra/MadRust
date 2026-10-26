#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Print what an initramfs archive holds, entry by entry.

The archive is the piece that decides what a guest can do: the program the
kernel runs, the device nodes the program needs and the model files it serves.
When a guest runs and says nothing, the archive is one of the two things that
can be wrong, and this script shows exactly what it holds.

Usage: bench/initramfs-entries.py [ARCHIVE]

The archive is a `newc` cpio. The entries are printed with their type, their
mode and, for a device, the major and minor numbers: a `/dev/console` that is
a regular file instead of a character device is the kind of detail that costs
an afternoon.
"""

from __future__ import annotations

import sys
from pathlib import Path

# The type bits of the cpio mode field.
S_IFMT = 0o170000
S_IFREG = 0o100000
S_IFCHR = 0o020000
S_IFDIR = 0o040000

KINDS = {
    S_IFREG: "file",
    S_IFCHR: "char",
    S_IFDIR: "dir",
}


def entries(data: bytes):
    """Yield (name, mode, major, minor, size) for every entry of the archive."""
    offset = 0
    while offset + 110 <= len(data):
        magic = bytes(data[offset : offset + 6])
        if magic not in (b"070701", b"070702"):
            raise SystemExit("initramfs-entries: the archive is not a newc cpio")
        fields = [
            int(data[offset + 6 + 8 * i : offset + 6 + 8 * (i + 1)], 16)
            for i in range(13)
        ]
        name_size = fields[11]
        file_size = fields[6]
        name_start = offset + 110
        name = bytes(data[name_start : name_start + name_size - 1]).decode(
            "utf-8", "replace"
        )
        data_start = (name_start + name_size + 3) & ~3
        yield name, fields[1], fields[9], fields[10], file_size
        if name == "TRAILER!!!":
            return
        offset = (data_start + file_size + 3) & ~3
    raise SystemExit("initramfs-entries: the archive has no trailer")


def main() -> int:
    archive = Path(sys.argv[1] if len(sys.argv) > 1 else "bench/initramfs.cpio")
    if not archive.is_file():
        raise SystemExit(f"initramfs-entries: no such archive: {archive}")
    data = archive.read_bytes()
    print(f"archive {archive} ({len(data)} bytes)")
    for name, mode, major, minor, size in entries(data):
        kind = KINDS.get(mode & S_IFMT, "other")
        line = f"{kind:5} {mode & 0o7777:04o} {size:>10} {name}"
        if kind == "char":
            line += f" major={major} minor={minor}"
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
