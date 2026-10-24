// SPDX-License-Identifier: GPL-2.0-only
//! Synthetic model generator for developing the runtime.
//!
//! The real weights are not available while the runtime is written, so this
//! development tool writes a `safetensors` file with the same tensor names and
//! shapes a Llama-style decoder uses, a `config.json` with the parameters the
//! shapes cannot carry, and a byte level `vocab.json`. The values come from a
//! fixed seed, so every run writes byte-for-byte identical files and the
//! generated token ids can be asserted as a golden value.
//!
//! This binary is not part of the runtime: it is built on the host by
//! `make infer-model` and its output is packed into the test initramfs.

#![deny(unsafe_op_in_unsafe_fn)]

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use madrust_infer::vocab::token_for_byte;

/// Fixed seed of the synthetic weights.
const SEED: u64 = 0x4d61_6452_7573_7421;

/// Prompt the synthetic model runs: the ASCII bytes of `MadRust`.
///
/// It lives in the generated `config.json`, not in the runtime, so the real
/// checkpoint can use a different prompt without rebuilding the binary.
const SYNTHETIC_PROMPT: [usize; 7] = [77, 97, 100, 82, 117, 115, 116];

/// Arguments with the dimensions the model is built from.
struct Args {
    hidden: usize,
    layers: usize,
    heads: usize,
    kv_heads: usize,
    ffn: usize,
    vocab: usize,
    eps: f32,
    theta: f32,
    interleaved: bool,
    out: PathBuf,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            hidden: 128,
            layers: 4,
            heads: 8,
            kv_heads: 4,
            ffn: 256,
            // Big enough for the reference prompt ids (the largest is 4649),
            // so the synthetic run exercises the same prompt constant as the
            // real model. The byte tokens still occupy ids 0 to 255.
            vocab: 8192,
            eps: 1e-5,
            theta: 10_000.0,
            interleaved: false,
            out: PathBuf::from("madrust-infer-model"),
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("madrust-infer-gen: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    if args.hidden == 0
        || args.layers == 0
        || args.heads == 0
        || args.kv_heads == 0
        || args.ffn == 0
        || args.vocab < 256
    {
        return Err("dimensions must be positive and vocab must cover 256 byte tokens".into());
    }
    if !args.hidden.is_multiple_of(args.heads) {
        return Err("hidden size must be divisible by the query head count".into());
    }
    if !args.heads.is_multiple_of(args.kv_heads) {
        return Err("query heads must be divisible by the key value head count".into());
    }
    let head_dim = args.hidden / args.heads;
    let kv_dim = args.kv_heads * head_dim;

    fs::create_dir_all(&args.out).map_err(|error| format!("cannot create output: {error}"))?;

    let mut rng = Rng::new(SEED);
    let mut tensors = Vec::new();
    let scale = 1.0 / (args.hidden as f32).sqrt();
    tensors.push(Tensor::random(
        "model.embed_tokens.weight".into(),
        vec![args.vocab, args.hidden],
        0.08,
        &mut rng,
    ));
    for layer in 0..args.layers {
        let prefix = format!("model.layers.{layer}");
        tensors.push(Tensor::random(
            format!("{prefix}.input_layernorm.weight"),
            vec![args.hidden],
            0.0,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.self_attn.q_proj.weight"),
            vec![args.hidden, args.hidden],
            scale,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.self_attn.k_proj.weight"),
            vec![kv_dim, args.hidden],
            scale,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.self_attn.v_proj.weight"),
            vec![kv_dim, args.hidden],
            scale,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.self_attn.o_proj.weight"),
            vec![args.hidden, args.hidden],
            scale,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.post_attention_layernorm.weight"),
            vec![args.hidden],
            0.0,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.mlp.gate_proj.weight"),
            vec![args.ffn, args.hidden],
            scale,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.mlp.up_proj.weight"),
            vec![args.ffn, args.hidden],
            scale,
            &mut rng,
        ));
        tensors.push(Tensor::random(
            format!("{prefix}.mlp.down_proj.weight"),
            vec![args.hidden, args.ffn],
            scale,
            &mut rng,
        ));
    }
    tensors.push(Tensor::random(
        "model.norm.weight".into(),
        vec![args.hidden],
        0.0,
        &mut rng,
    ));

    write_model(&args.out.join("model.safetensors"), &tensors)?;
    write_vocab(&args.out.join("vocab.json"), args.vocab)?;
    write_config(&args.out.join("config.json"), &args)?;

    let params: usize = tensors
        .iter()
        .map(|tensor| tensor.shape.iter().product::<usize>())
        .sum();
    println!(
        "madrust-infer-gen: wrote {} tensors, {} parameters, dims hidden={} layers={} q_heads={} kv_heads={} head_dim={} ffn={} vocab={}",
        tensors.len(),
        params,
        args.hidden,
        args.layers,
        args.heads,
        args.kv_heads,
        head_dim,
        args.ffn,
        args.vocab
    );
    Ok(())
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut iterator = std::env::args().skip(1);
    while let Some(argument) = iterator.next() {
        match argument.as_str() {
            "--hidden" => args.hidden = parse_usize(iterator.next(), "--hidden")?,
            "--layers" => args.layers = parse_usize(iterator.next(), "--layers")?,
            "--heads" => args.heads = parse_usize(iterator.next(), "--heads")?,
            "--kv-heads" => args.kv_heads = parse_usize(iterator.next(), "--kv-heads")?,
            "--ffn" => args.ffn = parse_usize(iterator.next(), "--ffn")?,
            "--vocab" => args.vocab = parse_usize(iterator.next(), "--vocab")?,
            "--eps" => args.eps = parse_f32(iterator.next(), "--eps")?,
            "--theta" => args.theta = parse_f32(iterator.next(), "--theta")?,
            "--interleaved" => args.interleaved = true,
            "--out" => {
                args.out = PathBuf::from(
                    iterator
                        .next()
                        .ok_or_else(|| "--out needs a directory".to_string())?,
                )
            }
            "--help" | "-h" => {
                println!(
                    "usage: madrust-infer-gen [--hidden N] [--layers N] [--heads N] \
                     [--kv-heads N] [--ffn N] [--vocab N] [--eps F] [--theta F] \
                     [--interleaved] [--out DIR]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(args)
}

fn parse_usize(value: Option<String>, name: &str) -> Result<usize, String> {
    value
        .ok_or_else(|| format!("{name} needs a value"))?
        .parse::<usize>()
        .map_err(|_| format!("{name} needs a non-negative integer"))
}

fn parse_f32(value: Option<String>, name: &str) -> Result<f32, String> {
    value
        .ok_or_else(|| format!("{name} needs a value"))?
        .parse::<f32>()
        .map_err(|_| format!("{name} needs a number"))
}

/// One tensor waiting to be written.
struct Tensor {
    name: String,
    shape: Vec<usize>,
    data: Vec<u16>,
}

impl Tensor {
    /// Random values scaled by `scale`, or norm weights near one when `scale`
    /// is zero.
    fn random(name: String, shape: Vec<usize>, scale: f32, rng: &mut Rng) -> Self {
        let count = shape.iter().product::<usize>();
        let mut data = Vec::with_capacity(count);
        if scale == 0.0 {
            for _ in 0..count {
                data.push(to_bf16(1.0 + 0.01 * rng.next_f32()));
            }
        } else {
            for _ in 0..count {
                data.push(to_bf16(scale * rng.next_f32()));
            }
        }
        Self { name, shape, data }
    }
}

/// Deterministic 64-bit linear congruential generator.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// A value in `[-1, 1)`.
    fn next_f32(&mut self) -> f32 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bits = (self.state >> 40) as u32;
        (bits as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    }
}

/// Round an `f32` to the nearest `bfloat16`.
fn to_bf16(value: f32) -> u16 {
    let bits = value.to_bits();
    let least_significant = (bits >> 16) & 1;
    bits.wrapping_add(0x7fff + least_significant)
        .wrapping_shr(16) as u16
}

/// Write the tensor directory and the raw data.
fn write_model(path: &PathBuf, tensors: &[Tensor]) -> Result<(), String> {
    let mut data = Vec::new();
    let mut header = String::from("{");
    for (index, tensor) in tensors.iter().enumerate() {
        if index != 0 {
            header.push(',');
        }
        let start = data.len();
        for value in &tensor.data {
            data.extend_from_slice(&value.to_le_bytes());
        }
        let end = data.len();
        header.push('"');
        header.push_str(&tensor.name);
        header.push_str("\":{\"dtype\":\"BF16\",\"shape\":[");
        for (position, dimension) in tensor.shape.iter().enumerate() {
            if position != 0 {
                header.push(',');
            }
            header.push_str(&dimension.to_string());
        }
        header.push_str("],\"data_offsets\":[");
        header.push_str(&start.to_string());
        header.push(',');
        header.push_str(&end.to_string());
        header.push_str("]}");
    }
    header.push('}');

    // Safetensors pads the JSON with spaces so the data starts on an eight byte
    // boundary. The runtime does not require it, but real files do it.
    while !header.len().is_multiple_of(8) {
        header.push(' ');
    }

    let mut file = File::create(path).map_err(|error| format!("cannot create model: {error}"))?;
    file.write_all(&(header.len() as u64).to_le_bytes())
        .and_then(|()| file.write_all(header.as_bytes()))
        .and_then(|()| file.write_all(&data))
        .map_err(|error| format!("cannot write model: {error}"))?;
    Ok(())
}

/// Write the byte level vocabulary.
fn write_vocab(path: &PathBuf, vocab_size: usize) -> Result<(), String> {
    let mut json = String::from("{");
    for id in 0..vocab_size {
        if id != 0 {
            json.push(',');
        }
        let token = if id < 256 {
            token_for_byte(id as u8)
        } else {
            format!("<extra{id}>")
        };
        json.push('"');
        push_escaped(&mut json, &token);
        json.push_str("\":");
        json.push_str(&id.to_string());
    }
    json.push('}');
    fs::write(path, json).map_err(|error| format!("cannot write vocabulary: {error}"))
}

/// Write the parameters the tensor shapes cannot carry.
fn write_config(path: &PathBuf, args: &Args) -> Result<(), String> {
    let prompt = SYNTHETIC_PROMPT
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let config = format!(
        "{{\"hidden_size\":{},\"num_hidden_layers\":{},\"num_attention_heads\":{},\"num_key_value_heads\":{},\"intermediate_size\":{},\"rms_norm_eps\":{},\"vocab_size\":{},\"tie_word_embeddings\":true,\"eos_token_id\":0,\"prompt_ids\":[{}],\"rope_theta\":{},\"rope_interleaved\":{}}}",
        args.hidden,
        args.layers,
        args.heads,
        args.kv_heads,
        args.ffn,
        args.eps,
        args.vocab,
        prompt,
        args.theta,
        args.interleaved
    );
    fs::write(path, config).map_err(|error| format!("cannot write config: {error}"))
}

/// Escape the characters JSON strings cannot carry raw.
fn push_escaped(out: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
}
