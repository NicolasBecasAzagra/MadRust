// SPDX-License-Identifier: GPL-2.0-only
//! x86_64 hardware access primitives for MadRust.
//!
//! Everything here is a thin, safe wrapper over one instruction or port access.
//! The crate compiles on non-x86_64 hosts as well so the workspace can be
//! checked and tested on a development machine; those stubs are never linked
//! into the kernel.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(test, allow(dead_code))]

#[cfg(test)]
extern crate std;

pub mod cpu;
pub mod pci;
pub mod port;

/// Re-export of the low level CPU helpers for convenience.
pub use cpu::{cpuid, cpuid_subleaf, hlt, Cpuid};
