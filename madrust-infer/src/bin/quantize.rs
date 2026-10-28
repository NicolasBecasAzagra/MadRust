// SPDX-License-Identifier: GPL-2.0-only
//! Quantize a `safetensors` checkpoint into the `Q4B32` block format.
//!
//! The conversion itself lives in [`madrust_infer::quantize`], because the gate
//! test uses the same code. This binary only parses the arguments and prints
//! the report, including the SHA-256 digest of the file it wrote.
//!
//! ```sh
//! cargo run --release --manifest-path madrust-infer/Cargo.toml \
//!     --bin madrust-infer-quantize -- --in MODEL --out MODEL-Q4
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;

use madrust_infer::quant;
use madrust_infer::quantize::Width;

fn main() {
    if let Err(error) = run() {
        eprintln!("madrust-infer-quantize: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (input, output, width) = parse_args()?;
    let report = madrust_infer::quantize::checkpoint(&input, &output, width)
        .map_err(|error| error.to_string())?;
    println!(
        "QUANT model={} bytes={} source_bytes={} tensors={} quantized={} kept={} bits={} block={} scale={}",
        report.model,
        report.bytes,
        report.source_bytes,
        report.tensors,
        report.quantized,
        report.kept,
        report.width.bits(),
        quant::BLOCK,
        quant::SCALE_NAME,
    );
    println!(
        "QUANT ratio={:.3} digest={}",
        report.ratio(),
        report.digest_hex()
    );
    Ok(())
}

fn parse_args() -> Result<(PathBuf, PathBuf, Width), String> {
    let mut input = None;
    let mut output = None;
    let mut width = Width::Four;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--in" => {
                input = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--in needs a directory".to_string())?,
                ))
            }
            "--out" => {
                output = Some(PathBuf::from(
                    arguments
                        .next()
                        .ok_or_else(|| "--out needs a directory".to_string())?,
                ))
            }
            "--bits" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| "--bits needs 4 or 8".to_string())?;
                width = match value.as_str() {
                    "4" => Width::Four,
                    "8" => Width::Eight,
                    other => return Err(format!("--bits must be 4 or 8, not {other}")),
                };
            }
            "--help" | "-h" => {
                println!(
                    "usage: madrust-infer-quantize [--bits 4|8] \
                     --in MODEL_DIR --out QUANTIZED_DIR"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let input = input.ok_or_else(|| "--in MODEL_DIR is required".to_string())?;
    let output = output.ok_or_else(|| "--out QUANTIZED_DIR is required".to_string())?;
    if !input.is_dir() {
        return Err(format!("{} is not a directory", input.display()));
    }
    Ok((input, output, width))
}
