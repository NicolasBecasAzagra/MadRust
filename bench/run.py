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
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

DEFAULT_ITERATIONS = 30

# The weights workload touches 65 536 pages per sample, so it prints fewer
# samples than the others and still finishes in a few seconds under emulation.
WEIGHTS_ITERATIONS = 5

# A 2 MiB fault is rare, so the huge run takes more samples to keep the slowest
# sample from being a single host preemption.
INFER_ITERATIONS = 1
BOOT_ITERATIONS = 1
# The agent loop answers once per run: the model decides how many steps it
# needs, and every run of the same task decides the same.
AGENT_ITERATIONS = 1
WEIGHTS_HUGE_ITERATIONS = 10

# Every workload the binary prints, with its unit and the number of samples one
# boot must produce. `weights` and `weights-huge` print two lines per sample:
# the cost per MiB and the slowest fault per page. The huge variant asks for
# 2 MiB pages with madvise before it touches the region.
WORKLOADS = {
    "syscall": ("ns/op", DEFAULT_ITERATIONS),
    "mmap": ("ns/MiB", DEFAULT_ITERATIONS),
    "weights": ("ns/MiB", WEIGHTS_ITERATIONS),
    "weights-tail": ("ns/page", WEIGHTS_ITERATIONS),
    "weights-huge": ("ns/MiB", WEIGHTS_HUGE_ITERATIONS),
    "weights-huge-tail": ("ns/page", WEIGHTS_HUGE_ITERATIONS),
    "futex": ("ns/roundtrip", DEFAULT_ITERATIONS),
    "threads": ("ns/total", DEFAULT_ITERATIONS),
    "infer-ttft": ("ms", INFER_ITERATIONS),
    "infer-tps": ("tok/s", INFER_ITERATIONS),
    "agent-steps": ("steps", AGENT_ITERATIONS),
    "agent-tokens": ("tokens", AGENT_ITERATIONS),
    "agent-seconds": ("s", AGENT_ITERATIONS),
    "agent-step-ms": ("ms/step", AGENT_ITERATIONS),
    "boot-seconds": ("s", BOOT_ITERATIONS),
    "boot-rss-mb": ("MiB", BOOT_ITERATIONS),
}

# The inference measurements only exist when the initramfs carries the runtime,
# so a run without it must not report them as missing.
OPTIONAL_WORKLOADS = {
    "infer-ttft",
    "infer-tps",
    "agent-steps",
    "agent-tokens",
    "agent-seconds",
    "agent-step-ms",
}

INFER_RE = re.compile(
    r"^INFER\s+(?P<workload>ttft_ms|tok_per_s)\s+(?P<value>-?\d+(?:\.\d+)?)\s*$"
)

# The agent loop reports the whole run on one line: how many steps it took, how
# many tokens the model generated, how long it ran and how many tool calls it
# made. The rig turns that into four measurements.
AGENT_RE = re.compile(
    r"^AGENT\s+steps=(?P<steps>\d+)\s+tokens=(?P<tokens>\d+)\s+"
    r"seconds=(?P<seconds>-?\d+(?:\.\d+)?)\s+tool_calls=(?P<tool_calls>\d+)\s*$"
)

RESULT_RE = re.compile(
    r"^RESULT\s+(?P<workload>\S+)\s+(?:(?P<error>ERR)\s+)?(?P<value>-?\d+)(?:\s+(?P<unit>\S+))?\s*$"
)

# Mainline can print a bracketed kernel log line onto the same serial line as a
# sample. The tail is not part of the sample, so cut it before matching.
KERNEL_LOG_RE = re.compile(r"\s*\[[^\]]*\].*$")

# The same kernel log line can land in front of the sample instead of behind
# it: the kernel writes to the same port as the program, and the two
# interleave at the byte level. A sample that arrives with the kernel prefix
# in front of it is still a sample, so the prefix is cut too.
LEADING_KERNEL_LOG_RE = re.compile(r"^\[[^\]]*\]\s*")

# The marker that says the run reached its end, with or without a kernel
# prefix in front of it.
BENCH_DONE_RE = re.compile(r"^\[[^\]]*\]\s*BENCH DONE\s*$")


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
    """Build the initramfs and check that it holds what a guest needs.

    The archive is always rebuilt, like the kernel and the runtime: a run
    that measures a stale archive measures a program that is not the one in
    the tree. The archive also carries the device nodes the guest needs, and
    a console that is not a character device takes the output of the
    program to a file instead of the serial port, which is a run with no
    output at all and no explanation.
    """
    script = repo_root() / "bench" / "mkinitramfs.sh"
    print(f"[rig] building initramfs {path}", file=sys.stderr)
    result = subprocess.run([str(script), str(path)], check=False)
    if result.returncode != 0 or not path.is_file():
        raise SystemExit("cannot build the initramfs")
    check_initramfs(path)


def check_initramfs(path: Path, strict: bool = True) -> None:
    """Check that the archive can carry the output of the program.

    The console has to be a character device with the numbers of the console
    (5:1). Anything else and the program writes into a file that nobody
    reads, and the run looks like a guest that says nothing.

    An archive the rig built has to pass the check. An archive the caller
    brought is only reported: the rig does not know how that guest gets its
    console, and it refuses to guess.
    """
    data = path.read_bytes()
    console = None
    offset = 0
    while offset + 110 <= len(data):
        magic = bytes(data[offset : offset + 6])
        if magic not in (b"070701", b"070702"):
            break
        fields = [
            int(data[offset + 6 + 8 * i : offset + 6 + 8 * (i + 1)], 16)
            for i in range(13)
        ]
        name_size = fields[11]
        file_size = fields[6]
        name_start = offset + 110
        name = bytes(data[name_start : name_start + name_size - 1])
        if name in (b"./dev/console", b"dev/console"):
            console = (fields[1], fields[9], fields[10])
        data_start = (name_start + name_size + 3) & ~3
        if name == b"TRAILER!!!":
            break
        offset = (data_start + file_size + 3) & ~3
    problem = ""
    if console is None:
        problem = "has no /dev/console"
    else:
        mode, major, minor = console
        if mode & 0o170000 != 0o020000 or major != 5 or minor != 1:
            problem = (
                f"has a /dev/console that is not the console device: "
                f"mode={mode & 0o170000:o} major={major} minor={minor}"
            )
    if not problem:
        return
    if strict:
        raise SystemExit(
            f"the initramfs {path} {problem}: the program would run without a "
            "console and its output would go nowhere"
        )
    print(
        f"[rig] WARNING: the initramfs {path} {problem}: if the program writes "
        "to the console, its output will not reach the serial port",
        file=sys.stderr,
    )


def qemu_command(
    qemu: str,
    image: Path,
    initramfs: Path,
    log: Path,
    accel: str,
    append_extra: str = "",
    memory: str = "512M",
    smp: str = "1",
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
        "-m",
        memory,
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
    # The number of processors is only given when it is not the default, so
    # the command line that the report shows has no repeated argument.
    if smp != "1":
        command[5:5] = ["-smp", smp]
    if accel == "kvm":
        command.insert(1, "-enable-kvm")
    elif accel == "hvf":
        command[1:1] = ["-accel", "hvf"]
    return command


def expected_complete(text: str, expected: dict[str, int]) -> bool:
    """Whether every measurement the run was asked for is in the log.

    The rig stops the guest as soon as it has the numbers, instead of
    waiting for the guest to leave. A guest that does not leave on its own
    (the reference kernel panics when PID 1 exits, and the power off does
    not always reach the machine) would otherwise cost the whole timeout
    after the data is already there.
    """
    seen: dict[str, int] = {}
    for raw_line in text.splitlines():
        line = LEADING_KERNEL_LOG_RE.sub("", raw_line.strip())
        match = RESULT_RE.match(KERNEL_LOG_RE.sub("", line))
        if match:
            name = match.group("workload")
            seen[name] = seen.get(name, 0) + 1
            continue
        match = INFER_RE.match(KERNEL_LOG_RE.sub("", line))
        if match:
            name = "infer-ttft" if match.group("workload") == "ttft_ms" else "infer-tps"
            seen[name] = seen.get(name, 0) + 1
            continue
        match = AGENT_RE.match(KERNEL_LOG_RE.sub("", line))
        if match:
            for name in ("agent-steps", "agent-tokens", "agent-seconds", "agent-step-ms"):
                seen[name] = seen.get(name, 0) + 1
    for name, count in expected.items():
        if name in ("boot-seconds", "boot-rss-mb"):
            # The rig measures those itself, after the run.
            continue
        if seen.get(name, 0) < count:
            return False
    return True


def bench_done(text: str) -> bool:
    """Whether the log says the workload reached the end of the run.

    The marker can arrive with a kernel log line in front of it, because the
    kernel and the program write to the same serial port.
    """
    for raw_line in text.splitlines():
        line = LEADING_KERNEL_LOG_RE.sub("", raw_line.strip())
        if BENCH_DONE_RE.match(line) or line == "BENCH DONE":
            return True
    return False


def read_log(path: Path) -> str:
    try:
        return path.read_text(errors="replace")
    except FileNotFoundError:
        return ""


def command_available(tool: str) -> bool:
    """Whether a tool is on the path."""
    return shutil.which(tool) is not None


def peak_rss_kb(pid: int) -> int:
    """Peak resident memory of a running process, in kilobytes.

    The value comes from the kernel of the host, which keeps the high water
    mark, so it is a peak and not a sample. It is what an operator sees on the
    machine, and it is the same measurement for every kernel.
    """
    try:
        with open(f"/proc/{pid}/status", errors="replace") as handle:
            for line in handle:
                if line.startswith("VmHWM:"):
                    return int(line.split()[1])
    except (FileNotFoundError, ProcessLookupError, ValueError, IndexError):
        return 0
    return 0


def run_once(
    label: str,
    image: Path,
    initramfs: Path,
    out_dir: Path,
    qemu: str,
    accel: str,
    timeout: float,
    append_extra: str = "",
    memory: str = "512M",
    smp: str = "1",
    expected: dict[str, int] | None = None,
) -> tuple[list[str], str, float, int, dict[str, Any]]:
    log = out_dir / f"{label}.log"
    # The output of QEMU itself is kept in a file of its own. When the guest
    # does not boot, the reason is in that stream and not in the serial log of
    # the guest, so throwing it away hides the only explanation there is.
    qemu_log = out_dir / f"{label}.qemu.log"
    command = qemu_command(qemu, image, initramfs, log, accel, append_extra, memory, smp)
    print(f"[rig] {label}: {' '.join(shlex.quote(part) for part in command)}", file=sys.stderr)

    with log.open("wb"):
        pass
    with qemu_log.open("wb") as handle:
        process = subprocess.Popen(
            command,
            stdout=handle,
            stderr=subprocess.STDOUT,
        )

    started = time.monotonic()
    peak_rss = 0
    deadline = started + timeout
    outcome = "timeout"
    finished_at = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            outcome = "exited"
            finished_at = time.monotonic()
            break
        text = read_log(log)
        if bench_done(text):
            # The last line is already in the file. The time of that line is
            # the time of the run, so it is taken before the guest is stopped.
            finished_at = time.monotonic()
            outcome = "finished"
            # Let the serial line flush the last line, then stop the guest.
            time.sleep(1.0)
            break
        if expected and expected_complete(text, expected):
            # Every measurement is in the log: the guest has nothing left to
            # say, and it does not always leave on its own.
            finished_at = time.monotonic()
            outcome = "finished"
            time.sleep(1.0)
            break
        peak_rss = max(peak_rss, peak_rss_kb(process.pid))
        time.sleep(0.2)

    elapsed = (finished_at if finished_at is not None else time.monotonic()) - started
    peak_rss = max(peak_rss, peak_rss_kb(process.pid))

    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    else:
        process.wait()

    text = read_log(log)
    for line in text.splitlines():
        if line.startswith("ADV "):
            print(f"[rig] {label}: {line}", file=sys.stderr)

    reason = ""
    if outcome == "timeout":
        reason = f"the guest did not finish in {timeout:.0f} s"
    elif not bench_done(text):
        reason = (
            f"the guest stopped before the end of the run "
            f"(qemu exit status {process.returncode})"
        )
    return command, text, elapsed, peak_rss, {
        "outcome": outcome,
        "reason": reason,
        "qemu": read_log(qemu_log),
    }


def parse_results(
    label: str,
    text: str,
    expected: dict[str, int],
    boot_rows: list[dict[str, Any]] | None = None,
    failure_reason: str = "",
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    seen = {name: 0 for name in WORKLOADS}

    for raw_line in text.splitlines():
        line = LEADING_KERNEL_LOG_RE.sub("", raw_line.strip())
        match = RESULT_RE.match(KERNEL_LOG_RE.sub("", line))
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

    # The inference runtime prints its measurements with its own prefix.
    for raw_line in text.splitlines():
        line = LEADING_KERNEL_LOG_RE.sub("", raw_line.strip())
        match = INFER_RE.match(KERNEL_LOG_RE.sub("", line))
        if not match:
            continue
        name = "infer-ttft" if match.group("workload") == "ttft_ms" else "infer-tps"
        rows.append(
            {
                "kernel": label,
                "workload": name,
                "iteration": 0,
                "value": match.group("value"),
                "unit": WORKLOADS[name][0],
                "status": "ok",
                "error": "",
            }
        )

    # The agent loop reports the whole run on one line.
    for raw_line in text.splitlines():
        line = LEADING_KERNEL_LOG_RE.sub("", raw_line.strip())
        match = AGENT_RE.match(KERNEL_LOG_RE.sub("", line))
        if not match:
            continue
        steps = int(match.group("steps"))
        seconds = float(match.group("seconds"))
        values = {
            "agent-steps": steps,
            "agent-tokens": int(match.group("tokens")),
            "agent-seconds": seconds,
            "agent-step-ms": (seconds * 1000.0 / steps) if steps else 0.0,
        }
        for name, value in values.items():
            rows.append(
                {
                    "kernel": label,
                    "workload": name,
                    "iteration": 0,
                    "value": f"{value:.3f}" if isinstance(value, float) else str(value),
                    "unit": WORKLOADS[name][0],
                    "status": "ok",
                    "error": "",
                }
            )

    # The measurements the rig takes itself are samples of their workload.
    for row in boot_rows or []:
        seen[row["workload"]] = seen.get(row["workload"], 0) + 1
        rows.append(row)

    # A workload that produced fewer lines than the guest was asked for did not
    # finish. Record the missing samples instead of silently shrinking the set,
    # and say why: the guest never reached the end, or the workload stopped
    # early.
    guest_finished = bench_done(text)
    reason = (
        ""
        if guest_finished
        else failure_reason or "the guest did not reach the end of the run"
    )
    for workload, (unit, _) in WORKLOADS.items():
        if workload in OPTIONAL_WORKLOADS and seen[workload] == 0:
            continue
        for iteration in range(seen[workload], expected.get(workload, 0)):
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
    parser.add_argument(
        "--madrust-variant",
        action="append",
        metavar="NAME=IMAGE",
        help=(
            "another MadRust kernel image to measure beside the current one, "
            "repeatable. The label is 'madrust-NAME', so an older build can be "
            "compared with the new one in a single run."
        ),
    )
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
    parser.add_argument(
        "--only",
        default="",
        help=(
            "comma separated list of the workloads this run measures. The rest "
            "are not expected and are not reported as missing, which is what a "
            "run with an initramfs that carries only the inference runtime "
            "needs."
        ),
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=1800.0,
        help="seconds per boot; several emulated processors make it slower",
    )
    parser.add_argument(
        "--smp",
        default="1",
        help="number of processors of the guest; both kernels get the same",
    )
    parser.add_argument(
        "--memory",
        default="512M",
        help="guest memory; the model needs at least 1024M",
    )
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
        # The kernel is built here when it is missing, so a host that has only
        # cloned the repository can measure. A checkout that was updated and
        # keeps an old image would measure the wrong kernel, so the build is
        # always run.
        print("[rig] building the MadRust kernel", file=sys.stderr)
        if not command_available("cargo"):
            raise SystemExit(
                "cargo is needed to build the kernel: "
                "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs "
                "| sh -s -- -y --profile minimal"
            )
        subprocess.run(["make", "build"], cwd=repo_root(), check=True)
        madrust_image = Path(args.madrust or default_madrust_image()).resolve()
    if not madrust_image.is_file():
        raise SystemExit(f"MadRust image not found: {madrust_image}")
    else:
        # Always rebuild, for the same reason as the runtime.
        subprocess.run(["make", "build"], cwd=repo_root(), check=True)

    kernels: list[tuple[str, Path, str]] = [("madrust", madrust_image, "")]
    for variant in args.madrust_variant or []:
        name, _, image = variant.partition("=")
        name = name.strip()
        if not name or not image:
            raise SystemExit(f"invalid --madrust-variant: {variant}")
        variant_image = Path(image).resolve()
        if not variant_image.is_file():
            raise SystemExit(f"MadRust variant image not found: {variant_image}")
        kernels.append((f"madrust-{name}", variant_image, ""))
    if not args.madrust_only:
        if not args.mainline:
            raise SystemExit("--mainline is required unless --madrust-only is set")
        mainline_image = Path(args.mainline).resolve()
        if not mainline_image.is_file():
            raise SystemExit(
                f"mainline image not found: {mainline_image}\n"
                "  the reference kernel is built with bench/prepare-mainline.sh, "
                "or any Linux kernel with a PVH note works"
            )
        kernels.append(("mainline", mainline_image, ""))
        for variant in args.mainline_variant or []:
            name, _, extra = variant.partition("=")
            name = name.strip()
            if not name:
                raise SystemExit(f"invalid --mainline-variant: {variant}")
            kernels.append((f"mainline-{name}", mainline_image, extra.strip()))

    if args.initrd:
        # The caller brought its own archive: it is the one that carries the
        # checkpoint or the agent, and the rig does not touch it. Rebuilding
        # it would replace it with the benchmark workload, and the run would
        # measure something else than what was asked for.
        initrd = Path(args.initrd).resolve()
        if not initrd.is_file():
            raise SystemExit(f"the initramfs {initrd} does not exist")
        check_initramfs(initrd, strict=False)
        print(
            f"[rig] using the initramfs {initrd} "
            f"({initrd.stat().st_size} bytes), as it was given",
            file=sys.stderr,
        )
    else:
        initrd = root / "bench" / "initramfs.cpio"
        ensure_initramfs(initrd)

    out_dir = Path(args.out)
    if not out_dir.is_absolute():
        out_dir = root / out_dir
    out_dir.mkdir(parents=True, exist_ok=True)

    expected = {
        name: args.iterations if args.iterations is not None else samples
        for name, (_, samples) in WORKLOADS.items()
    }

    only = {name.strip() for name in args.only.split(",") if name.strip()}
    if only:
        unknown = only - set(WORKLOADS)
        if unknown:
            raise SystemExit(f"unknown workloads: {', '.join(sorted(unknown))}")
        expected = {name: count for name, count in expected.items() if name in only}

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
        command, text, elapsed, peak_rss, outcome = run_once(
            label,
            image,
            initrd,
            out_dir,
            args.qemu,
            args.accel,
            args.timeout,
            append_extra,
            args.memory,
            args.smp,
            expected,
        )
        metadata["commands"][label] = shlex.join(command)
        # What a boot costs, measured from the outside: the wall clock of the
        # whole run and the resident memory the process reached. The time is
        # only a measurement when the run reached its end; a run that ran into
        # the timeout says nothing about the cost of a boot.
        measured = outcome["outcome"] != "timeout"
        boot_rows = [
            {
                "kernel": label,
                "workload": "boot-seconds",
                "iteration": 0,
                "value": f"{elapsed:.3f}" if measured else "",
                "unit": "s",
                "status": "ok" if measured else "error",
                "error": "" if measured else outcome["reason"],
            },
            {
                "kernel": label,
                "workload": "boot-rss-mb",
                "iteration": 0,
                "value": f"{peak_rss / 1024:.1f}" if peak_rss else "",
                "unit": "MiB",
                "status": "ok" if peak_rss else "error",
                "error": ""
                if peak_rss
                else "this host has no /proc, so the memory is not measured",
            },
        ]
        rows = parse_results(label, text, expected, boot_rows, outcome["reason"])
        write_csv(out_dir / f"{label}.csv", rows)
        all_rows.extend(rows)
        ok = sum(1 for row in rows if row["status"] == "ok")
        errors = sum(1 for row in rows if row["status"] == "error")
        print(
            f"[rig] {label}: {ok} samples, {errors} errors, {len(rows)} rows recorded",
            file=sys.stderr,
        )
        # The marker belongs to the benchmark workload. A run that measures the
        # inference runtime does not print it, and its absence is not a fault.
        if not bench_done(text) and any(
            name in expected for name in WORKLOADS if name not in OPTIONAL_WORKLOADS
        ):
            print(f"[rig] {label}: serial log does not contain BENCH DONE", file=sys.stderr)
        if outcome["reason"]:
            # The reason the run failed belongs in the report, and the output
            # of QEMU is where the reason usually is.
            print(f"[rig] {label}: {outcome['reason']}", file=sys.stderr)
            for line in outcome["qemu"].splitlines()[-20:]:
                print(f"[rig] {label}: qemu: {line}", file=sys.stderr)

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
