// SPDX-License-Identifier: GPL-2.0-only
//! Device drivers for MadRust.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod debug_exit;
pub mod uart;
