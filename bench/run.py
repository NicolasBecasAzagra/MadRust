#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""Run the MadRust benchmark workload on one or two kernel images.

The rig boots each kernel with identical QEMU flags, the same initramfs and the
same command line. The workload inside the guest prints one RESULT line per
sample and BENCH DONE at the end; this script records every line, including the
ones that report an error, and writes:

  <out>/<kernel>.csv    one row per iteration
  <out>/summary.csv     median, p99 and standard deviation per workload
  <out>/summary.json    the same table plus the run metadata
  <out>/<kernel>.log    the serial log

The QEMU flags are fixed on purpose. The CPU model is never `host`, because it
changes between machines and invalidates the comparison, and both kernels see
the same machine type, CPU count, memory size and initramfs.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import re
import shlex
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

DEFAULT_ITERATIONS = 30

# The weights workload touches 65 536 pages per sample, so it prints fewer
# samples than the others and still finishes in a few seconds under emulation.
WEIGHTS_ITERATIONS = 5

# Every workload the binary prints, with its unit and the number of samples one
# boot must produce. `weights` prints two lines per sample: the cost per MiB and
# the slowest single page.
WORKLOADS = {
    "syscall": ("ns/op", DEFAULT_ITERATIONS),
    "mmap": ("ns/MiB", DEFAULT_ITERATIONS),
    "weights": ("ns/MiB", WEIGHTS_ITERATIONS),
    "weights-tail": ("ns/page", WEIGHTS_ITERATIONS),
    "futex": ("ns/roundtrip", DEFAULT_ITERATIONS),
    "threads": ("ns/total", DEFAULT_ITERATIONS),
}

RESULT_RE = re.compile(
    r"^RESULT\s+(?P<workload>\S+)\s+(?:(?P<error>ERR)\s+)?(?P<value>-?\d+)(?:\s+(?P<unit>\S+))?\s*$"
)

# Mainline can print a bracketed kernel log line onto the same serial line as a
# sample. The tail is not part of the sample, so cut it before matching.
KERNEL_LOG_RE = re.compile(r"\s*\[[^\]]*\].*$")


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def qemu_version(qemu: str) -> str:
    try:
        result = subprocess.run(
            [qemu, "--version"], capture_output=True, text=True, check=False
        )
    except OSError as error:
        return f"unavailable ({error})"
    first = result.stdout.splitlines()[0] if result.stdout else ""
    return first or f"unknown (status {result.returncode})"


def host_metadata() -> dict[str, str]:
    uname = platform.uname()
    return {
        "hostname": uname.node,
        "platform": platform.platform(),
        "machine": uname.machine,
        "system": uname.system,
        "release": uname.release,
        "python": platform.python_version(),
    }


def default_madrust_image() -> str:
    root = repo_root()
    result = subprocess.run(
        ["make", "-s", "--no-print-directory", "print-kernel-path"],
        cwd=root,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        raise SystemExit(
            "cannot ask the Makefile for the kernel path: " + result.stderr.strip()
        )
    return result.stdout.strip()


def ensure_initramfs(path: Path) -> None:
    if path.is_file() and path.stat().st_size > 0:
        return
    script = repo_root() / "bench" / "mkinitramfs.sh"
    print(f"[rig] building initramfs {path}", file=sys.stderr)
    result = subprocess.run([str(script), str(path)], check=False)
    if result.returncode != 0 or not path.is_file():
        raise SystemExit("cannot build the initramfs")


def qemu_command(
    qemu: str,
    image: Path,
    initramfs: Path,
    log: Path,
    accel: str,
    append_extra: str = "",
) -> list[str]:
    kernel_args = "console=ttyS0"
    if append_extra:
        kernel_args = f"{kernel_args} {append_extra}"
    command = [
        qemu,
        "-M",
        "q35",
        "-cpu",
        "Skylake-Client",
        "-smp",
        "1",
        "-m",
        "512M",
        "-kernel",
        str(image),
        "-initrd",
        str(initramfs),
        "-append",
        kernel_args,
        "-display",
        "none",
        "-monitor",
        "none",
        "-no-reboot",
        "-serial",
        f"file:{log}",
        "-device",
        "isa-debug-exit,iobase=0xf4,iosize=0x04",
    ]
    if accel == "kvm":
        command.insert(1, "-enable-kvm")
    elif accel == "hvf":
        command[1:1] = ["-accel", "hvf"]
    return command


def read_log(path: Path) -> str:
    try:
        return path.read_text(errors="replace")
    except FileNotFoundError:
        return ""


def run_once(
    label: str,
    image: Path,
    initramfs: Path,
    out_dir: Path,
    qemu: str,
    accel: str,
    timeout: float,
    append_extra: str = "",
) -> tuple[list[str], str]:
    log = out_dir / f"{label}.log"
    command = qemu_command(qemu, image, initramfs, log, accel, append_extra)
    print(f"[rig] {label}: {' '.join(shlex.quote(part) for part in command)}", file=sys.stderr)

    with log.open("wb"):
        pass
    process = subprocess.Popen(
        command,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    deadline = time.monotonic() + timeout
    finished = False
    while time.monotonic() < deadline:
        if process.poll() is not None:
            finished = True
            break
        if "BENCH DONE" in read_log(log):
            # Let the serial line flush the last line, then stop the guest.
            time.sleep(1.0)
            finished = True
            break
        time.sleep(0.2)

    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    elif not finished:
        process.wait()

    return command, read_log(log)


def parse_results(label: str, text: str, expected: dict[str, int]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    seen = {name: 0 for name in WORKLOADS}

    for raw_line in text.splitlines():
        match = RESULT_RE.match(KERNEL_LOG_RE.sub("", raw_line.strip()))
        if not match:
            continue
        workload = match.group("workload")
        if workload not in WORKLOADS:
            continue
        iteration = seen[workload]
        seen[workload] = iteration + 1
        unit = match.group("unit") or WORKLOADS[workload][0]
        error = match.group("error")
        if error:
            rows.append(
                {
                    "kernel": label,
                    "workload": workload,
                    "iteration": iteration,
                    "value": "",
                    "unit": unit,
                    "status": "error",
                    "error": match.group("value"),
                }
            )
        else:
            rows.append(
                {
                    "kernel": label,
                    "workload": workload,
                    "iteration": iteration,
                    "value": match.group("value"),
                    "unit": unit,
                    "status": "ok",
                    "error": "",
                }
            )

    # A workload that produced fewer lines than the guest was asked for did not
    # finish. Record the missing samples instead of silently shrinking the set,
    # and say why: the guest never reached the end, or the workload stopped
    # early.
    guest_finished = "BENCH DONE" in text
    reason = "" if guest_finished else "the guest did not reach the end of the run"
    for workload, (unit, _) in WORKLOADS.items():
        for iteration in range(seen[workload], expected[workload]):
            rows.append(
                {
                    "kernel": label,
                    "workload": workload,
                    "iteration": iteration,
                    "value": "",
                    "unit": unit,
                    "status": "missing",
                    "error": reason or "the workload printed fewer samples than expected",
                }
            )
    return rows


def write_csv(path: Path, rows: list[dict[str, Any]]) -> None:
    columns = ["kernel", "workload", "iteration", "value", "unit", "status", "error"]
    with path.open("w", newline="") as handle:
        handle.write(",".join(columns) + "\n")
        for row in rows:
            handle.write(",".join(str(row[column]) for column in columns) + "\n")


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


def summarise(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    table: list[dict[str, Any]] = []
    kernels = sorted({row["kernel"] for row in rows})
    for kernel in kernels:
        for workload, (unit, _) in WORKLOADS.items():
            selected = [
                row
                for row in rows
                if row["kernel"] == kernel and row["workload"] == workload
            ]
            values = [float(row["value"]) for row in selected if row["status"] == "ok"]
            errors = sum(1 for row in selected if row["status"] == "error")
            missing = sum(1 for row in selected if row["status"] == "missing")
            table.append(
                {
                    "kernel": kernel,
                    "workload": workload,
                    "unit": unit,
                    "samples": len(values),
                    "errors": errors,
                    "missing": missing,
                    "median": statistics.median(values) if values else None,
                    "p99": percentile(values, 0.99) if values else None,
                    "stddev": statistics.pstdev(values) if len(values) > 1 else None,
                }
            )
    return table


def format_number(value: float | None) -> str:
    if value is None:
        return ""
    return f"{value:.2f}"


def write_summary(out_dir: Path, table: list[dict[str, Any]], metadata: dict[str, Any]) -> None:
    columns = [
        "kernel",
        "workload",
        "unit",
        "samples",
        "errors",
        "missing",
        "median",
        "p99",
        "stddev",
    ]
    with (out_dir / "summary.csv").open("w", newline="") as handle:
        handle.write(",".join(columns) + "\n")
        for row in table:
            handle.write(
                ",".join(
                    format_number(row[column]) if column in {"median", "p99", "stddev"} else str(row[column])
                    for column in columns
                )
                + "\n"
            )
    with (out_dir / "summary.json").open("w") as handle:
        json.dump({"metadata": metadata, "summary": table}, handle, indent=2)
        handle.write("\n")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--madrust", help="MadRust kernel image")
    parser.add_argument("--mainline", help="mainline Linux kernel image")
    parser.add_argument(
        "--mainline-variant",
        action="append",
        metavar="NAME[=KERNEL_ARGS]",
        help=(
            "configuration of the mainline image, repeatable. The name labels the "
            "run and the optional arguments are added to the kernel command line, "
            "for example 'nomitig=mitigations=off'. The default configuration is "
            "always measured as well."
        ),
    )
    parser.add_argument(
        "--madrust-only",
        action="store_true",
        help="run only the MadRust image, before a mainline image exists",
    )
    parser.add_argument("--initrd", help="cpio archive to hand to both kernels")
    parser.add_argument("--out", default="bench/results", help="result directory")
    parser.add_argument(
        "--iterations",
        type=int,
        default=None,
        help=(
            "override the samples the workload is expected to print per "
            "workload, instead of the per-workload defaults"
        ),
    )
    parser.add_argument("--timeout", type=float, default=180.0, help="seconds per boot")
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    parser.add_argument(
        "--accel",
        choices=["tcg", "kvm", "hvf"],
        default="tcg",
        help="QEMU accelerator; both kernels always use the same one",
    )
    args = parser.parse_args()

    root = repo_root()
    madrust_image = Path(args.madrust or default_madrust_image()).resolve()
    if not madrust_image.is_file():
        raise SystemExit(f"MadRust image not found: {madrust_image}")

    kernels: list[tuple[str, Path, str]] = [("madrust", madrust_image, "")]
    if not args.madrust_only:
        if not args.mainline:
            raise SystemExit("--mainline is required unless --madrust-only is set")
        mainline_image = Path(args.mainline).resolve()
        if not mainline_image.is_file():
            raise SystemExit(f"mainline image not found: {mainline_image}")
        kernels.append(("mainline", mainline_image, ""))
        for variant in args.mainline_variant or []:
            name, _, extra = variant.partition("=")
            name = name.strip()
            if not name:
                raise SystemExit(f"invalid --mainline-variant: {variant}")
            kernels.append((f"mainline-{name}", mainline_image, extra.strip()))

    initrd = Path(args.initrd).resolve() if args.initrd else root / "bench" / "initramfs.cpio"
    ensure_initramfs(initrd)

    out_dir = Path(args.out)
    if not out_dir.is_absolute():
        out_dir = root / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    expected = {
        name: args.iterations if args.iterations is not None else samples
        for name, (_, samples) in WORKLOADS.items()
    }

    metadata: dict[str, Any] = {
        "qemu": qemu_version(args.qemu),
        "accel": args.accel,
        "host": host_metadata(),
        "initrd": {"path": str(initrd), "sha256": sha256(initrd)},
        "kernels": {},
        "commands": {},
        "iterations_expected": expected,
        "started": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }

    all_rows: list[dict[str, Any]] = []
    for label, image, append_extra in kernels:
        metadata["kernels"][label] = {
            "path": str(image),
            "sha256": sha256(image),
            "kernel_args": append_extra or "console=ttyS0",
        }
        command, text = run_once(
            label,
            image,
            initrd,
            out_dir,
            args.qemu,
            args.accel,
            args.timeout,
            append_extra,
        )
        metadata["commands"][label] = shlex.join(command)
        rows = parse_results(label, text, expected)
        write_csv(out_dir / f"{label}.csv", rows)
        all_rows.extend(rows)
        ok = sum(1 for row in rows if row["status"] == "ok")
        errors = sum(1 for row in rows if row["status"] == "error")
        print(
            f"[rig] {label}: {ok} samples, {errors} errors, {len(rows)} rows recorded",
            file=sys.stderr,
        )
        if "BENCH DONE" not in text:
            print(f"[rig] {label}: serial log does not contain BENCH DONE", file=sys.stderr)

    table = summarise(all_rows)
    write_summary(out_dir, table, metadata)

    print(f"[rig] raw results: {out_dir}")
    print(f"[rig] summary:     {out_dir / 'summary.csv'}")
    print(f"[rig] metadata:    {out_dir / 'summary.json'}")
    for row in table:
        print(
            f"{row['kernel']:9} {row['workload']:8} "
            f"median={format_number(row['median']) or '-':>12} "
            f"p99={format_number(row['p99']) or '-':>12} "
            f"stddev={format_number(row['stddev']) or '-':>10} "
            f"n={row['samples']} errors={row['errors']} missing={row['missing']}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
