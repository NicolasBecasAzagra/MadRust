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

# The user program is a separate workspace and a normal Rust program built
# against the standard library of the Linux musl target. That target is
# precompiled, so no `-Z build-std` is needed and no C toolchain is involved:
# rust-lld links the static position independent executable. The kernel embeds
# the copy in madrust-userland/madrust-userland.elf, so it must be built first.
USER_TARGET := x86_64-unknown-linux-musl
USERLAND_ELF := madrust-userland/madrust-userland.elf
USERLAND_RUSTFLAGS := -Z unstable-options -C linker=rust-lld -C linker-flavor=gnu-lld
USERLAND_FLAGS := --manifest-path madrust-userland/Cargo.toml \
	--target $(USER_TARGET) \
	--target-dir $(TARGET_DIR)/userland

# The benchmark workload is a second standard library program, built exactly
# like the userland one. The rig puts the copy into an uncompressed cpio
# archive instead of linking it into the kernel.
BENCH_ELF := madrust-bench/madrust-bench.elf
BENCH_FLAGS := --manifest-path madrust-bench/Cargo.toml \
	--target $(USER_TARGET) \
	--target-dir $(TARGET_DIR)/bench

# The network client is a third standard library program, built exactly like the
# workload. The rig packs it into its own initramfs next to the configuration
# that names the server, so the same bytes run on both kernels.
NETCLIENT_ELF := madrust-netclient/madrust-netclient.elf
NETCLIENT_FLAGS := --manifest-path madrust-netclient/Cargo.toml \
	--target $(USER_TARGET) \
	--target-dir $(TARGET_DIR)/netclient

# The inference runtime is a fourth standard library program, built exactly like
# the workload. Its synthetic model, vocabulary and config are packed into the
# same kind of initramfs, so the kernel runs it unmodified.
INFER_DIR := madrust-infer
INFER_ELF := $(INFER_DIR)/madrust-infer.elf
INFER_TARGET_DIR := $(TARGET_DIR)/infer
INFER_FLAGS := --manifest-path $(INFER_DIR)/Cargo.toml \
	--target $(USER_TARGET) \
	--target-dir $(INFER_TARGET_DIR)
INFER_MODEL_DIR := $(TARGET_DIR)/infer-model
INFER_INITRAMFS := $(INFER_DIR)/initramfs.cpio

# The agent is a fourth standard library program, built like the runtime and
# packed with the same model so the rig runs the same bytes on both kernels.
AGENT_DIR := madrust-agent
AGENT_ELF := $(AGENT_DIR)/madrust-agent.elf
AGENT_TARGET_DIR := $(TARGET_DIR)/agent
AGENT_FLAGS := --manifest-path $(AGENT_DIR)/Cargo.toml \
	--target $(USER_TARGET) \
	--target-dir $(AGENT_TARGET_DIR)
AGENT_INITRAMFS := $(AGENT_DIR)/initramfs.cpio

# The reader of the cpio archive of the initramfs is a crate of its own, like
# the other programs that are not the kernel.
INITRAMFS_DIR := madrust-initramfs

# The protocols of the network path are a crate of their own too, but they are
# not a program: the kernel depends on them, so they are built for the kernel
# target, and their tests run on the host.
NET_DIR := madrust-net

QEMU ?= qemu-system-x86_64
# Number of processors for the multiprocessor boot.
SMP ?= 4
QEMU_FLAGS := -M q35 -m 512M \
	-kernel $(KERNEL) \
	-display none -monitor none -no-reboot \
	-serial stdio \
	-device isa-debug-exit,iobase=0xf4,iosize=0x04

.PHONY: all build run run-smp test fmt fmt-check clippy clippy-host check clean userland bench netclient print-kernel-path infer infer-model infer-initramfs agent agent-initramfs run-infer

# The path of the built kernel, so the CI does not have to repeat it.
print-kernel-path:
	@echo $(KERNEL)

all: build

# The kernel embeds the user program, so it is always built first. The copy
# into the repository is what the kernel embeds: the linker writes through
# mmap, which is not reliable on every file system, while a plain copy is.
# The user programs are standard library programs for Linux musl, so the
# precompiled standard library of that target has to exist for the pinned
# toolchain. Installing it here means a fresh machine builds without a manual
# step.
musl-std:
	@rustup target list --installed --toolchain $(TOOLCHAIN) | grep -qx $(USER_TARGET) || \
		rustup target add --toolchain $(TOOLCHAIN) $(USER_TARGET)

userland: musl-std
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) build --release $(USERLAND_FLAGS)
	cp $(TARGET_DIR)/userland/$(USER_TARGET)/release/madrust-userland $(USERLAND_ELF)

# Static workload binary for the benchmark rig.
# The features of the workload. The CI builds it with "small" so that the boot
# with the initramfs finishes in time under emulation.
BENCH_FEATURES ?=

bench:
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) build --release $(BENCH_FLAGS) $(if $(BENCH_FEATURES),--features $(BENCH_FEATURES),)
	cp $(TARGET_DIR)/bench/$(USER_TARGET)/release/madrust-bench $(BENCH_ELF)

# Static network client for the rig and for the gate of the pipeline. The rig
# packs it with the configuration that names the server, so the archive is built
# per run and not here.
netclient: musl-std
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) build --release $(NETCLIENT_FLAGS)
	cp $(TARGET_DIR)/netclient/$(USER_TARGET)/release/madrust-netclient $(NETCLIENT_ELF)

# The inference runtime, built with the same rust-lld recipe as the workload so
# the same bytes can run on MadRust and on mainline Linux.
infer: musl-std
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) build --release $(INFER_FLAGS)
	cp $(INFER_TARGET_DIR)/$(USER_TARGET)/release/madrust-infer $(INFER_ELF)

# The synthetic model is a development tool, built and run on the host. Its
# output lives outside the repository with the rest of the build products.
infer-model:
	$(CARGO) run --release --manifest-path $(INFER_DIR)/Cargo.toml \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/infer-host \
		--bin madrust-infer-gen -- --out $(INFER_MODEL_DIR)

# One uncompressed cpio archive carrying the runtime at /bench and /init, plus
# the model, vocabulary and config the runtime opens by path.
infer-initramfs: infer infer-model
	EXTRA_DIR=$(INFER_MODEL_DIR) bench/mkinitramfs.sh $(INFER_INITRAMFS) $(INFER_ELF)

# The tool calling agent over the runtime, built with the same rust-lld recipe
# and packed with the same model. The default task reads /vocab.json from the
# initramfs, so the archive carries the runtime files unchanged.
agent: musl-std
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) build --release $(AGENT_FLAGS)
	cp $(AGENT_TARGET_DIR)/$(USER_TARGET)/release/madrust-agent $(AGENT_ELF)

agent-initramfs: agent infer-model
	EXTRA_DIR=$(INFER_MODEL_DIR) bench/mkinitramfs.sh $(AGENT_INITRAMFS) $(AGENT_ELF)

run-infer: infer-initramfs build
	@$(QEMU) -M q35 -m 512M \
		-kernel $(KERNEL) -initrd $(INFER_INITRAMFS) \
		-append console=ttyS0 \
		-display none -monitor none -no-reboot \
		-serial stdio \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04; status=$$?; \
	if [ $$status -eq 33 ]; then \
		echo "boot OK (QEMU exit status 33)"; \
	else \
		echo "unexpected QEMU exit status $$status"; exit $$status; \
	fi

build: userland infer
	$(CARGO) build --release $(KERNEL_FLAGS)

# The multiprocessor boot. The kernel finds the processors in the ACPI table
# and starts them, so this is the only way to exercise that path.
run-smp: build
	@$(QEMU) -M q35 -cpu Skylake-Client -smp $(SMP) -m 512M \
		-kernel $(KERNEL) \
		-display none -monitor none -no-reboot \
		-serial stdio \
		-device isa-debug-exit,iobase=0xf4,iosize=0x04; status=$$?; \
	if [ $$status -eq 33 ]; then \
		echo "boot OK with $(SMP) processors (QEMU exit status 33)"; \
	else \
		echo "unexpected QEMU exit status $$status"; exit $$status; \
	fi

run: build
	@$(QEMU) $(QEMU_FLAGS); status=$$?; \
	if [ $$status -eq 33 ]; then \
		echo "boot OK (QEMU exit status 33)"; \
	else \
		echo "unexpected QEMU exit status $$status"; exit $$status; \
	fi

test:
	$(CARGO) test --workspace --target $(HOST_TARGET) --target-dir $(TARGET_DIR) $(HOST_ONLY)
	$(CARGO) test --manifest-path $(INFER_DIR)/Cargo.toml --lib \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/infer-host
	$(CARGO) test --manifest-path $(AGENT_DIR)/Cargo.toml \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/agent-host
	$(CARGO) test --manifest-path $(INITRAMFS_DIR)/Cargo.toml \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/initramfs-host
	$(CARGO) test --manifest-path $(NET_DIR)/Cargo.toml \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/net-host
	$(CARGO) test --manifest-path madrust-netclient/Cargo.toml \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/netclient-host


fmt:
	$(CARGO) fmt --all
	$(CARGO) fmt --manifest-path madrust-userland/Cargo.toml
	$(CARGO) fmt --manifest-path madrust-bench/Cargo.toml
	$(CARGO) fmt --manifest-path $(INFER_DIR)/Cargo.toml
	$(CARGO) fmt --manifest-path $(AGENT_DIR)/Cargo.toml
	$(CARGO) fmt --manifest-path $(INITRAMFS_DIR)/Cargo.toml
	$(CARGO) fmt --manifest-path $(NET_DIR)/Cargo.toml
	$(CARGO) fmt --manifest-path madrust-netclient/Cargo.toml

fmt-check:
	$(CARGO) fmt --all -- --check
	$(CARGO) fmt --manifest-path madrust-userland/Cargo.toml -- --check
	$(CARGO) fmt --manifest-path madrust-bench/Cargo.toml -- --check
	$(CARGO) fmt --manifest-path $(INFER_DIR)/Cargo.toml -- --check
	$(CARGO) fmt --manifest-path $(AGENT_DIR)/Cargo.toml -- --check
	$(CARGO) fmt --manifest-path $(INITRAMFS_DIR)/Cargo.toml -- --check
	$(CARGO) fmt --manifest-path $(NET_DIR)/Cargo.toml -- --check
	$(CARGO) fmt --manifest-path madrust-netclient/Cargo.toml -- --check

# The kernel build reads the embedded user program, so both lint targets need
# the userland binary to exist.
clippy: userland
	$(CARGO) clippy --workspace --bins --lib $(KERNEL_FLAGS) -- -D warnings
	$(CARGO) clippy --manifest-path $(NET_DIR)/Cargo.toml --lib $(KERNEL_FLAGS) -- -D warnings
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) clippy $(USERLAND_FLAGS) -- -D warnings
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) clippy $(BENCH_FLAGS) -- -D warnings
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) clippy $(INFER_FLAGS) --bins --lib -- -D warnings
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) clippy $(AGENT_FLAGS) --bins -- -D warnings
	RUSTFLAGS="$(USERLAND_RUSTFLAGS)" $(CARGO) clippy $(NETCLIENT_FLAGS) --bins -- -D warnings

clippy-host:
	$(CARGO) clippy --workspace --all-targets --target $(HOST_TARGET) \
		--target-dir $(TARGET_DIR) $(HOST_ONLY) -- -D warnings
	$(CARGO) clippy --manifest-path $(INFER_DIR)/Cargo.toml --all-targets \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/infer-host -- -D warnings
	$(CARGO) clippy --manifest-path $(AGENT_DIR)/Cargo.toml --all-targets \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/agent-host -- -D warnings
	$(CARGO) clippy --manifest-path $(INITRAMFS_DIR)/Cargo.toml --all-targets \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/initramfs-host -- -D warnings
	$(CARGO) clippy --manifest-path $(NET_DIR)/Cargo.toml --all-targets \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/net-host -- -D warnings
	$(CARGO) clippy --manifest-path madrust-netclient/Cargo.toml --all-targets \
		--target $(HOST_TARGET) --target-dir $(TARGET_DIR)/netclient-host -- -D warnings


check: userland
	$(CARGO) check --workspace --bins --lib $(KERNEL_FLAGS)

clean:
	$(CARGO) clean --target-dir $(TARGET_DIR)
	rm -rf $(TARGET_DIR) $(USERLAND_ELF) $(BENCH_ELF) $(NETCLIENT_ELF) $(INFER_ELF) $(INFER_INITRAMFS) \
		$(AGENT_ELF) $(AGENT_INITRAMFS)
