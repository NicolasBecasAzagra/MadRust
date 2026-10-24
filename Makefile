# SPDX-License-Identifier: GPL-2.0-only
#
# MadRust build entry points. Works on macOS and Linux.

CARGO ?= cargo
TOOLCHAIN := $(shell sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)
CARGO := $(CARGO) +$(TOOLCHAIN)

TARGET := x86_64-madrust
KERNEL_TARGET := $(TARGET).json
# The build output lives outside the repository by default: the checkout can sit
# on an exFAT volume, where the linker writes through mmap and the file system
# can leave large artifacts zeroed. Override TARGET_DIR to put it elsewhere.
TARGET_DIR ?= $(HOME)/.cache/madrust/target
KERNEL := $(TARGET_DIR)/$(TARGET)/release/madrust-kernel
HOST_TARGET := $(shell rustc -vV | sed -n 's/^host: //p')

# The bare-metal target has no precompiled core, so it is built from source.
# These flags stay on the command line instead of .cargo/config.toml to keep
# the host toolchain usable for `cargo test` and `cargo clippy`.
KERNEL_FLAGS := --target $(KERNEL_TARGET) \
	--target-dir $(TARGET_DIR) \
	-Z build-std=core,alloc,compiler_builtins \
	-Z build-std-features=compiler-builtins-mem

# The kernel binary is `no_std` + `no_main`: it cannot be built as a host test
# target and is excluded from the host runs.
HOST_ONLY := --exclude madrust-kernel

# The user program is a separate workspace: it is linked at 0x400000 with its
# own target and linker script, and the kernel embeds the result. It must be
# built before the kernel, whose `include_bytes!` reads the copy in
# madrust-userland/madrust-userland.elf.
USER_TARGET := x86_64-madrust-user
USERLAND_ELF := madrust-userland/madrust-userland.elf
USERLAND_FLAGS := --manifest-path madrust-userland/Cargo.toml \
	--target $(USER_TARGET).json \
	--target-dir $(TARGET_DIR)/userland \
	-Z build-std=core

QEMU ?= qemu-system-x86_64
QEMU_FLAGS := -M q35 -m 512M \
	-kernel $(KERNEL) \
	-display none -monitor none -no-reboot \
	-serial stdio \
	-device isa-debug-exit,iobase=0xf4,iosize=0x04

.PHONY: all build run test fmt fmt-check clippy clippy-host check clean userland

all: build

# The kernel embeds the user program, so it is always built first. The copy
# into the repository is what the kernel embeds: the linker writes through
# mmap, which is not reliable on every file system, while a plain copy is.
userland:
	$(CARGO) build --release $(USERLAND_FLAGS)
	cp $(TARGET_DIR)/userland/$(USER_TARGET)/release/madrust-userland $(USERLAND_ELF)

build: userland
	$(CARGO) build --release $(KERNEL_FLAGS)

run: build
	@$(QEMU) $(QEMU_FLAGS); status=$$?; \
	if [ $$status -eq 33 ]; then \
		echo "boot OK (QEMU exit status 33)"; \
	else \
		echo "unexpected QEMU exit status $$status"; exit $$status; \
	fi

test:
	$(CARGO) test --workspace --target $(HOST_TARGET) --target-dir $(TARGET_DIR) $(HOST_ONLY)

fmt:
	$(CARGO) fmt --all
	$(CARGO) fmt --manifest-path madrust-userland/Cargo.toml

fmt-check:
	$(CARGO) fmt --all -- --check
	$(CARGO) fmt --manifest-path madrust-userland/Cargo.toml -- --check

# The kernel build reads the embedded user program, so both lint targets need
# the userland binary to exist.
clippy: userland
	$(CARGO) clippy --workspace --bins --lib $(KERNEL_FLAGS) -- -D warnings
	$(CARGO) clippy $(USERLAND_FLAGS) -- -D warnings

clippy-host:
	$(CARGO) clippy --workspace --all-targets --target $(HOST_TARGET) \
		--target-dir $(TARGET_DIR) $(HOST_ONLY) -- -D warnings

check: userland
	$(CARGO) check --workspace --bins --lib $(KERNEL_FLAGS)

clean:
	$(CARGO) clean --target-dir $(TARGET_DIR)
	rm -rf $(TARGET_DIR) $(USERLAND_ELF)
