# Contributing to MadRust

MadRust is a learning-first kernel project with a public audience: the code is
read by other people trying to understand how an operating system works, and
by maintainers judging whether the project deserves ecosystem attention.

## Ground rules

- English for code, comments, commit messages and documentation.
- Every source file starts with `// SPDX-License-Identifier: GPL-2.0-only`.
- Every `unsafe` block or function is preceded by a `// SAFETY:` comment that
  states the invariant the code relies on and why it holds.
- `#![deny(unsafe_op_in_unsafe_fn)]` in every crate.
- No `unwrap()`, `expect()` or `panic!()` on kernel paths. Handle errors
  explicitly, even if the only recovery is to print and halt.
- No `todo!()`, no dead code, no commented-out blocks. Dead code is deleted.
- Dependencies limited to MIT, BSD and ISC licensed crates. `Apache-2.0-only`
  crates are forbidden in the kernel because they are incompatible with
  `GPL-2.0-only`.

## Commits

Conventional Commits, one logical change per commit:

```text
feat(arch): add 64-bit entry trampoline for PVH boot
fix(serial): drop the third byte of the divisor latch write
docs(boot): document the start_info layout mismatch
```

Keep commits small enough to be reviewed in one sitting. Do not mix a refactor
with a behaviour change.

## Before opening a pull request

```sh
make check      # cargo fmt --check + cargo clippy -D warnings
make test       # host unit tests
make run        # boot in QEMU, confirm the serial banner
```

CI runs the same commands plus a headless QEMU boot test that asserts the
serial log contains `MadRust: boot OK` and that QEMU exited with status 33
(the `isa-debug-exit` device encodes the exit code).

## Toolchain

The nightly is pinned in `rust-toolchain.toml`. Nightly is required because the
kernel builds `core` and `compiler_builtins` from source with `-Z build-std`.
Bump the pin in a dedicated commit, never mixed with feature work.

## Scripts

Every script under `bench/` and `scripts/` is committed with the executable
bit set, and a clone has to be able to run it.

The working copy of this repository can live on a file system that does not
keep that bit, in which case a script added there is recorded as one that
cannot be run. When that happens, set the mode in the index instead of on
disk:

```sh
git update-index --chmod=+x bench/native_comparison.sh
```

Scripts are also written to run through the interpreter, so they keep working
either way:

```sh
sh bench/native_comparison.sh
```

## Style

`cargo fmt` defaults, plus `clippy::all` as an error. Keep hardware access
inside `madrust-arch`; keep parsing and pure logic in crates that can be unit
tested on the host. Logic that can be tested on the host must be tested on the
host.

## No filler

This is a kernel that other people read to learn. Every line must be
explainable by the author. Avoid boilerplate that exists only to look
complete, avoid restating the code in comments, and avoid adding abstractions
for a single use.

## Reporting bugs

Open an issue with the QEMU command line, the serial log, the toolchain
version and the smallest reproduction you have. Kernel bugs are much easier to
fix with a deterministic serial transcript.
