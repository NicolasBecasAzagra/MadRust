// SPDX-License-Identifier: GPL-2.0-only
//! Microbenchmark of the blocked matrix multiplication, without a guest.
//!
//! The shapes are the projection shapes of the reference SmolLM2-135M
//! decoder: thirty layers with their query, key, value, output, gate, up and
//! down matrices, plus the output projection, which reuses the embedding
//! matrix when the checkpoint is tied. The weights are synthetic and
//! deterministic, but their number, their layout and their `bfloat16` encoding
//! are the ones the runtime streams, so the rate here and the rate the runtime
//! prints during a real generation describe the same work.
//!
//! The binary is a normal host program. It reads no model and calls no kernel:
//! it allocates the matrices, runs the kernel, and reports multiply
//! accumulates per second and floating point operations per second. `--kernel
//! scalar` measures the path the runtime used before the blocked kernel, and
//! `--kernel both` measures one after the other on the same data, which is the
//! before and after number for one machine.
//!
//! ```sh
//! cargo run --release --manifest-path madrust-infer/Cargo.toml \
//!     --target "$(rustc -vV | sed -n 's/^host: //p')" \
//!     --bin madrust-infer-matmul -- --kernel both --iterations 5
//! ```

#![deny(unsafe_op_in_unsafe_fn)]

use std::hint::black_box;
use std::time::Instant;

use madrust_infer::config::Config;
use madrust_infer::error::{Error, Result};
use madrust_infer::matvec;

/// Arguments of one measurement.
struct Args {
    iterations: usize,
    kernel: Kernel,
    /// Reuse one allocation per shape instead of one per decoder layer.
    ///
    /// The default allocates the whole reference model, which is what the
    /// runtime streams. A guest with a small anonymous memory budget can pass
    /// this flag to measure the same shapes in a cache hot loop.
    reuse: bool,
}

/// Which kernel to measure.
#[derive(Clone, Copy)]
enum Kernel {
    Auto,
    Scalar,
    Both,
}

/// One projection shape and the copies of it a token walks.
struct Group {
    name: &'static str,
    rows: usize,
    columns: usize,
    /// Copies of this shape in one token, which may exceed `matrices.len()`.
    copies: usize,
    matrices: Vec<Vec<u8>>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("madrust-infer-matmul: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut args = parse_args()?;
    if args.iterations == 0 {
        return Err(Error::new("--iterations must be positive"));
    }
    // The guest build has one allocation per shape but still walks every copy.
    args.reuse = args.reuse || cfg!(feature = "small");
    let groups = groups(args.reuse)?;
    check(&groups)?;
    match args.kernel {
        Kernel::Auto => measure("auto", &groups, &args, Kernel::Auto)?,
        Kernel::Scalar => measure("scalar", &groups, &args, Kernel::Scalar)?,
        Kernel::Both => {
            measure("scalar", &groups, &args, Kernel::Scalar)?;
            measure("auto", &groups, &args, Kernel::Auto)?;
        }
    }
    Ok(())
}

/// Build the matrices of one reference token.
fn groups(reuse: bool) -> Result<Vec<Group>> {
    let config = Config::defaults();
    let hidden = config.hidden_size;
    let kv_dim = config.num_key_value_heads * (hidden / config.num_attention_heads);
    let ffn = config.intermediate_size;
    let vocab = config.vocab_size;
    let layers = config.num_hidden_layers;
    let shapes = [
        ("q_proj", hidden, hidden, layers),
        ("k_proj", kv_dim, hidden, layers),
        ("v_proj", kv_dim, hidden, layers),
        ("o_proj", hidden, hidden, layers),
        ("gate_proj", ffn, hidden, layers),
        ("up_proj", ffn, hidden, layers),
        ("down_proj", hidden, ffn, layers),
        // The tied checkpoint reuses the embedding matrix as `lm_head`.
        ("output", vocab, hidden, 1),
    ];
    let mut groups = Vec::with_capacity(shapes.len());
    for (name, rows, columns, copies) in shapes {
        let allocations = if reuse { 1 } else { copies };
        let mut matrices = Vec::with_capacity(allocations);
        for _ in 0..allocations {
            matrices.push(filled(rows * columns)?);
        }
        groups.push(Group {
            name,
            rows,
            columns,
            copies,
            matrices,
        });
    }
    Ok(groups)
}

/// Deterministic `bfloat16` weights in a narrow range.
fn filled(elements: usize) -> Result<Vec<u8>> {
    let bytes = elements
        .checked_mul(2)
        .ok_or_else(|| Error::new("matrix byte length overflow"))?;
    let mut out = Vec::with_capacity(bytes);
    let mut state = 0x4d61_6452_7573_7421u64;
    for _ in 0..elements {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let value = (((state >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0) * 0.05;
        let bits = value.to_bits();
        let rounded = bits.wrapping_add(0x7fff + ((bits >> 16) & 1));
        out.extend_from_slice(&(rounded >> 16).to_le_bytes());
    }
    Ok(out)
}

/// Compare the selected vector path with the scalar path on every shape.
///
/// The runtime checks its tokens, but a change that is still inside the token
/// margin can hide a broken lane. This check is cheap next to the measurement
/// and makes the benchmark say which kernel it actually measured.
fn check(groups: &[Group]) -> Result<()> {
    let input_size = groups.iter().map(|group| group.columns).max().unwrap_or(0);
    let output_size = groups.iter().map(|group| group.rows).max().unwrap_or(0);
    let input: Vec<f32> = (0..input_size)
        .map(|index| ((index as f32) * 0.017 - 0.4).sin())
        .collect();
    let mut scalar = vec![0.0f32; output_size];
    let mut vector = vec![0.0f32; output_size];
    let mut worst = 0.0f32;
    for group in groups {
        let Some(matrix) = group.matrices.first() else {
            continue;
        };
        matvec::matvec_scalar(
            matrix,
            group.rows,
            group.columns,
            &input[..group.columns],
            &mut scalar[..group.rows],
        );
        matvec::matvec_bf16(
            matrix,
            group.rows,
            group.columns,
            &input[..group.columns],
            &mut vector[..group.rows],
        )?;
        for (left, right) in vector[..group.rows].iter().zip(&scalar[..group.rows]) {
            worst = worst.max((left - right).abs());
        }
    }
    let tolerance = 1e-4 * input_size as f32;
    if worst > tolerance {
        return Err(Error::new(format!(
            "the vector path differs from the scalar path by {worst}, tolerance {tolerance}"
        )));
    }
    println!(
        "MATMUL check selected={} max_diff={worst:.6} tolerance={tolerance:.6}",
        matvec::kernel_name()
    );
    Ok(())
}

/// Run one configuration and print the rate per shape and in total.
fn measure(name: &str, groups: &[Group], args: &Args, kernel: Kernel) -> Result<()> {
    let input_size = groups
        .iter()
        .map(|group| group.columns)
        .max()
        .ok_or_else(|| Error::new("no shapes to measure"))?;
    let output_size = groups
        .iter()
        .map(|group| group.rows)
        .max()
        .ok_or_else(|| Error::new("no shapes to measure"))?;
    let input: Vec<f32> = (0..input_size)
        .map(|index| ((index as f32) * 0.017 - 0.4).sin())
        .collect();
    let mut output = vec![0.0f32; output_size];

    let mut total_macs = 0u64;
    let mut total_nanos = 0u128;
    let mut per_group = Vec::with_capacity(groups.len());
    for _ in 0..args.iterations {
        for group in groups {
            let mut macs = 0u64;
            let mut nanos = 0u128;
            let repetitions = if args.reuse { group.copies } else { 1 };
            for _ in 0..repetitions {
                for matrix in &group.matrices {
                    let started = Instant::now();
                    match kernel {
                        Kernel::Scalar => matvec::matvec_scalar(
                            matrix,
                            group.rows,
                            group.columns,
                            &input[..group.columns],
                            &mut output[..group.rows],
                        ),
                        Kernel::Auto | Kernel::Both => matvec::matvec_bf16(
                            matrix,
                            group.rows,
                            group.columns,
                            &input[..group.columns],
                            &mut output[..group.rows],
                        )?,
                    }
                    nanos += started.elapsed().as_nanos();
                    macs += matvec::macs(group.rows, group.columns);
                    black_box(&output[..group.rows]);
                }
            }
            per_group.push((group, macs, nanos));
            total_macs += macs;
            total_nanos += nanos;
        }
    }
    let seconds = total_nanos as f64 / 1_000_000_000.0;
    if seconds <= 0.0 {
        return Err(Error::new("the clock did not advance"));
    }
    let rate = total_macs as f64 / seconds;
    println!(
        "MATMUL kernel={name} selected={} arch={} iterations={} macc={} flops={} seconds={:.6} macc_per_s={:.0} flops_per_s={:.0}",
        matvec::kernel_name(),
        std::env::consts::ARCH,
        args.iterations,
        total_macs,
        total_macs.saturating_mul(2),
        seconds,
        rate,
        2.0 * rate
    );
    // Sum the samples of one shape across iterations before reporting its rate.
    let mut totals: Vec<(&'static str, usize, usize, usize, u64, u128)> = Vec::new();
    for (group, macs, nanos) in per_group {
        match totals.iter_mut().find(|entry| entry.0 == group.name) {
            Some(entry) => {
                entry.4 += macs;
                entry.5 += nanos;
            }
            None => totals.push((
                group.name,
                group.rows,
                group.columns,
                group.copies,
                macs,
                nanos,
            )),
        }
    }
    for (name, rows, columns, copies, macs, nanos) in totals {
        let group_seconds = nanos as f64 / 1_000_000_000.0;
        let group_rate = if group_seconds > 0.0 {
            macs as f64 / group_seconds
        } else {
            0.0
        };
        println!(
            "MATMUL shape name={} rows={} cols={} copies={} macc_per_s={:.0}",
            name, rows, columns, copies, group_rate
        );
    }
    Ok(())
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        iterations: 3,
        kernel: Kernel::Auto,
        reuse: false,
    };
    let mut iterator = std::env::args().skip(1);
    while let Some(argument) = iterator.next() {
        match argument.as_str() {
            "--iterations" => {
                let value = iterator
                    .next()
                    .ok_or_else(|| Error::new("--iterations needs a value"))?;
                args.iterations = value
                    .parse::<usize>()
                    .map_err(|_| Error::new("--iterations needs a positive integer"))?;
            }
            "--kernel" => {
                let value = iterator
                    .next()
                    .ok_or_else(|| Error::new("--kernel needs a value"))?;
                args.kernel = match value.as_str() {
                    "auto" => Kernel::Auto,
                    "scalar" => Kernel::Scalar,
                    "both" => Kernel::Both,
                    _ => return Err(Error::new("--kernel must be auto, scalar or both")),
                };
            }
            "--reuse" => args.reuse = true,
            "--help" | "-h" => {
                println!(
                    "usage: madrust-infer-matmul [--iterations N] \
                     [--kernel auto|scalar|both] [--reuse]"
                );
                std::process::exit(0);
            }
            other => return Err(Error::new(format!("unknown argument {other}"))),
        }
    }
    Ok(args)
}
