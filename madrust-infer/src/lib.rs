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
//! * [`bpe`] is the byte pair merge encoder: the GPT-2 pre-tokenizer, the byte
//!   level alphabet and the `merges.txt` of a checkpoint, stored in one buffer
//!   so a real merge table fits a small heap. It is host tested and needs no
//!   file system.
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
//! * [`quant`] is the block format of a quantized checkpoint: four or eight
//!   bit weights in blocks of thirty-two with one binary16 scale, decoded
//!   inside the matrix vector product. It is host tested and
//!   `docs/QUANTIZATION.md` describes the file.
//! * [`quantize`] rewrites a `safetensors` checkpoint in that format, with the
//!   SHA-256 of the result. It is the code behind the `madrust-infer-quantize`
//!   tool and behind the gate test.
//! * [`sha256`] is the digest the quantizer prints, so the tools need no
//!   dependency either.
//! * [`cli`] parses the optional token count of the binary, eight by default.
//! * [`paths`] resolves the model root: the initramfs root in the guest, or
//!   `MADRUST_INFER_ROOT` when a native host process sets it.
//! * [`prompt`] encodes a prompt that the caller built, one byte per token,
//!   for the callers that build a conversation at run time.
//! * [`power_off`] asks the machine to go down before the last process
//!   leaves, so mainline Linux does not panic when PID 1 exits.
//! * [`reference`] is the published SmolLM2-135M run the runtime checks
//!   itself against when it serves that checkpoint.
//! * `mapping`, `model`, `runtime` and `compare` need Linux `mmap` and are only
//!   built for the Linux target. Everything above is host tested.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod bpe;
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
pub mod quant;
pub mod quantize;
pub mod reference;
pub mod safetensors;
pub mod sha256;
pub mod vocab;

#[cfg(target_os = "linux")]
pub mod compare;
#[cfg(target_os = "linux")]
pub mod mapping;
#[cfg(target_os = "linux")]
pub mod model;
#[cfg(target_os = "linux")]
pub mod runtime;

#[cfg(all(test, target_os = "linux"))]
mod gate;
