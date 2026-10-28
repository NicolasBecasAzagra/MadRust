// SPDX-License-Identifier: GPL-2.0-only
//! Compare a floating point checkpoint with its quantized copy.
//!
//! The tool decodes the same prompt greedily with both checkpoints and compares
//! the logits of every step where the two agree on the input. It prints the
//! tokens of both runs and the largest and mean absolute difference of the
//! logits, which is the evidence that a quantized checkpoint still answers the
//! same. The comparison runs inside the guest, because the decoder needs the
//! `mmap` path the runtime uses on MadRust and on Linux.
//!
//! The guest passes no arguments to its init, so the defaults are the file
//! names an initramfs carries: `/model.safetensors` for the floating point
//! checkpoint and `/model-q4.safetensors` for the quantized one. A host run can
//! pass the three paths as arguments.
//!
//! ```sh
//! madrust-infer-compare [FLOAT_MODEL] [QUANTIZED_MODEL] [CONFIG]
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(target_os = "linux")]
fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let float_path = arguments
        .first()
        .cloned()
        .unwrap_or_else(|| "/model.safetensors".to_string());
    let quant_path = arguments
        .get(1)
        .cloned()
        .unwrap_or_else(|| "/model-q4.safetensors".to_string());
    let config_path = arguments
        .get(2)
        .cloned()
        .unwrap_or_else(|| madrust_infer::paths::CONFIG_PATH.to_string());
    let result = madrust_infer::compare::run(&float_path, &quant_path, &config_path);
    let status = match &result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("COMPARE ERR {error}");
            1
        }
    };
    std::io::Write::flush(&mut std::io::stdout()).ok();
    madrust_infer::power_off::power_off();
    std::process::exit(status);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("madrust-infer-compare: build the tool for x86_64-unknown-linux-musl");
    std::process::exit(2);
}
