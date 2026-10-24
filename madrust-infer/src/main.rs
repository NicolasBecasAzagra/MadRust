// SPDX-License-Identifier: GPL-2.0-only
//! The inference runtime binary.
//!
//! The prompt is already encoded because the runtime has no pre-tokenizer. A
//! model config can carry `prompt_ids`; otherwise the runtime uses the baked
//! reference prompt, so the same binary serves the small synthetic model and
//! the real checkpoint.

#![deny(unsafe_op_in_unsafe_fn)]

/// Tokens to generate after the prompt, the reference oracle default.
#[cfg(target_os = "linux")]
const GENERATED: usize = 8;

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = madrust_infer::runtime::run(GENERATED) {
        eprintln!("INFER ERR {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("madrust-infer: build the runtime for x86_64-unknown-linux-musl");
    std::process::exit(2);
}
