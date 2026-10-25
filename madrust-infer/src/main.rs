// SPDX-License-Identifier: GPL-2.0-only
//! The inference runtime binary.
//!
//! The prompt is already encoded because the runtime has no pre-tokenizer. A
//! model config can carry `prompt_ids`; otherwise the runtime uses the baked
//! reference prompt, so the same binary serves the small synthetic model and
//! the real checkpoint.
//!
//! An optional positional argument is the number of tokens to generate, eight
//! when it is absent. The guest passes no arguments, so its run is unchanged;
//! a native run can ask for more tokens to make its token rate stable. The
//! model root comes from `MADRUST_INFER_ROOT` when the environment defines it,
//! and from the initramfs root otherwise.

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
fn main() {
    let count = match madrust_infer::cli::token_count(std::env::args_os().skip(1)) {
        Ok(count) => count,
        Err(error) => {
            eprintln!("INFER ERR {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = madrust_infer::runtime::run(count) {
        eprintln!("INFER ERR {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("madrust-infer: build the runtime for x86_64-unknown-linux-musl");
    std::process::exit(2);
}
