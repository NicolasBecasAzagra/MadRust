// SPDX-License-Identifier: GPL-2.0-only
//! A dependency free Llama-style inference runtime.
//!
//! The runtime reads a `safetensors` model and a vocabulary from the
//! initramfs, maps the weights read only instead of copying them, and runs the
//! decoder one token at a time against a key value cache. The dimensions are
//! parameters: a bigger model is a change of shape in the config file and the
//! tensor names, not a change of architecture.
//!
//! ## Layout of this crate
//!
//! * [`json`] is the small scanner the other parsers share. It is written for
//!   the fixed shape of the files this runtime owns, not as a general JSON
//!   library.
//! * [`safetensors`] parses the tensor directory in a `safetensors` header.
//! * [`vocab`] inverts the token string to id map and decodes bytes.
//! * [`config`] reads the handful of values a `safetensors` file cannot carry:
//!   the head count, the norm epsilon, the RoPE theta and the RoPE pair order.
//! * [`math`] holds the pure kernels: RMSNorm, RoPE, softmax and greedy
//!   argmax.
//! * `mapping`, `model` and `runtime` need Linux `mmap` and are only built for
//!   the Linux target. Everything above is host tested.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod config;
pub mod error;
pub mod json;
pub mod math;
pub mod safetensors;
pub mod vocab;

#[cfg(target_os = "linux")]
pub mod mapping;
#[cfg(target_os = "linux")]
pub mod model;
#[cfg(target_os = "linux")]
pub mod runtime;
