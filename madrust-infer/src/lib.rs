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
//! * [`vocab`] inverts the token string to id map and decodes bytes. It also
//!   recovers the raw bytes of a token, which the grammar needs.
//! * [`config`] reads the handful of values a `safetensors` file cannot carry:
//!   the head count, the norm epsilon, the RoPE theta and the RoPE pair order.
//! * [`math`] holds the pure kernels: RMSNorm, RoPE, softmax and greedy
//!   argmax.
//! * [`grammar`] is the byte level automaton of a tool call. It answers which
//!   bytes may follow a state, whether a state completes the call, and which
//!   tokens of a vocabulary keep the automaton alive. It has no I/O, so it is
//!   host tested.
//! * [`chat`] writes the SmolLM2 chat prompt from plain strings, including the
//!   tool list, and checks the chat markers against a vocabulary.
//! * [`matvec`] is the blocked vector by matrix multiplication over
//!   `bfloat16` weights, with an AVX2 and FMA path, a NEON path, and a scalar
//!   path that all agree to floating point tolerance. It is host tested and
//!   the `madrust-infer-matmul` binary measures it without a model.
//! * [`cli`] parses the optional token count of the binary, eight by default.
//! * [`paths`] resolves the model root: the initramfs root in the guest, or
//!   `MADRUST_INFER_ROOT` when a native host process sets it.
//! * [`prompt`] encodes a prompt that the caller built, one byte per token,
//!   for the callers that build a conversation at run time.
//! * [`power_off`] asks the machine to go down before the last process
//!   leaves, so mainline Linux does not panic when PID 1 exits.
//! * [`reference`] is the published SmolLM2-135M run the runtime checks
//!   itself against when it serves that checkpoint.
//! * `mapping`, `model` and `runtime` need Linux `mmap` and are only built for
//!   the Linux target. Everything above is host tested.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod chat;
pub mod cli;
pub mod config;
pub mod error;
pub mod grammar;
pub mod json;
pub mod math;
pub mod matvec;
pub mod paths;
pub mod power_off;
pub mod prompt;
pub mod reference;
pub mod safetensors;
pub mod vocab;

#[cfg(target_os = "linux")]
pub mod mapping;
#[cfg(target_os = "linux")]
pub mod model;
#[cfg(target_os = "linux")]
pub mod runtime;
