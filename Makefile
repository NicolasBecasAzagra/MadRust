# SPDX-License-Identifier: GPL-2.0-only
#
# MadRust build entry points. Works on macOS and Linux.

CARGO ?= cargo
TOOLCHAIN := $(shell sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
CARGO := $(CARGO) +$(TOOLCHAIN)

TARGET := x86_64-madrust
KERNEL_TARGET := $(TARGET).json
KERNEL := target/$(TARGET)/release/madrust-kernel
HOST_TARGET := $(shell rustc -vV | sed -n 's/^host: //p')

# The bare-metal target has no precompiled core, so it is built from source.
# These flags stay on the command line instead of .cargo/config.toml to keep
# the host toolchain usable for `cargo test` and `cargo clippy`.
KERNEL_FLAGS := --target $(KERNEL_TARGET) \
	-Z build-std=core,alloc,compiler_builtins \
	-Z build-std-features=compiler-builtins-mem

# The kernel binary is `no_std` + `no_main`: it cannot be built as a host test
# target and is excluded from the host runs.
HOST_ONLY := --exclude madrust-kernel

QEMU ?= qemu-system-x86_64
QEMU_FLAGS := -M q35 -m 512M \
	-kernel $(KERNEL) \
	-display none -monitor none -no-reboot \
	-serial stdio \
	-device isa-debug-exit,iobase=0xf4,iosize=0x04

.PHONY: all build run test fmt fmt-check clippy clippy-host check clean

all: build

build:
	$(CARGO) build --release $(KERNEL_FLAGS)

run: build
	@$(QEMU) $(QEMU_FLAGS); status=$$?; \
	if [ $$status -eq 33 ]; then \
		echo "boot OK (QEMU exit status 33)"; \
	else \
		echo "unexpected QEMU exit status $$status"; exit $$status; \
	fi

test:
	$(CARGO) test --workspace --target $(HOST_TARGET) $(HOST_ONLY)

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

clippy:
	$(CARGO) clippy --workspace --bins --lib $(KERNEL_FLAGS) -- -D warnings

clippy-host:
	$(CARGO) clippy --workspace --all-targets --target $(HOST_TARGET) $(HOST_ONLY) -- -D warnings

check:
	$(CARGO) check --workspace --bins --lib $(KERNEL_FLAGS)

clean:
	$(CARGO) clean
