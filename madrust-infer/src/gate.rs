// SPDX-License-Identifier: GPL-2.0-only
//! The gate: the quantized development model answers like the floating one.
//!
//! The test writes a small checkpoint with the shape of the development model
//! (`madrust-infer-gen`), quantizes it with the same code the tool runs, binds
//! both files through the decoder, and compares the greedy tokens for the
//! published prompt. It needs no download, so the continuous integration runs
//! it with `make test` on every change; the kernel build of the guest is not
//! involved, because the decoder is a normal Linux program too.
//!
//! The test is gated on Linux because `model` and `mapping` are: the runtime
//! maps the checkpoint with the system calls Linux implements, and the host of
//! a developer may not be Linux.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::error::Result;
use crate::model::{Model, ModelFile};
use crate::quantize;
use crate::reference;

/// Dimensions of the development model, matching `madrust-infer-gen`.
const HIDDEN: usize = 128;
const LAYERS: usize = 4;
const HEADS: usize = 8;
const KV_HEADS: usize = 4;
const FFN: usize = 256;
const VOCAB: usize = 8192;

/// Tokens the gate compares. The published run is eight; four keep the debug
/// decoder under a second and still cross several layers.
const COUNT: usize = 4;

struct Checkpoint {
    name: String,
    shape: Vec<usize>,
    data: Vec<u16>,
}

impl Checkpoint {
    /// A matrix whose values the block format holds exactly.
    ///
    /// Every block of thirty-two weights carries a `+7` or a `-7`, so the scale
    /// of the block is exactly one and every integer in `-7..=7` is one nibble.
    /// The quantizer therefore reproduces the tensor bit for bit, and the gate
    /// compares the decoder against the floating point path instead of
    /// comparing two different models. A random checkpoint has no such
    /// property: its greedy answer can turn on a margin of `0.003`, which no
    /// four bit encoding can promise to keep.
    fn exact(name: &str, shape: Vec<usize>, state: &mut u64) -> Self {
        let count: usize = shape.iter().product();
        let mut data = vec![0u16; count];
        if shape.len() == 1 {
            // A norm weight stays a floating point vector and is copied as is.
            data.fill(to_bf16(1.0));
        } else {
            let columns = shape[1];
            for row in 0..shape[0] {
                for block in 0..columns / 32 {
                    let base = row * columns + block * 32;
                    data[base] = to_bf16(if block % 2 == 0 { 7.0 } else { -7.0 });
                    for index in 1..32 {
                        data[base + index] = to_bf16((next(state) * 7.0).round());
                    }
                }
            }
        }
        Self {
            name: name.to_string(),
            shape,
            data,
        }
    }
}

/// Deterministic values in `[-1, 1)`.
fn next(state: &mut u64) -> f32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    ((*state >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
}

/// Round an `f32` to the nearest `bfloat16`.
fn to_bf16(value: f32) -> u16 {
    let bits = value.to_bits();
    let least_significant = (bits >> 16) & 1;
    bits.wrapping_add(0x7fff + least_significant)
        .wrapping_shr(16) as u16
}

/// The tensors of the development model, in a form the format holds exactly.
fn tensors() -> Vec<Checkpoint> {
    let mut state = 0x4d61_6452_7573_7421u64;
    let head_dim = HIDDEN / HEADS;
    let kv_dim = KV_HEADS * head_dim;
    let mut tensors = vec![Checkpoint::exact(
        "model.embed_tokens.weight",
        vec![VOCAB, HIDDEN],
        &mut state,
    )];
    for layer in 0..LAYERS {
        let prefix = format!("model.layers.{layer}");
        let names: [String; 9] = [
            format!("{prefix}.input_layernorm.weight"),
            format!("{prefix}.self_attn.q_proj.weight"),
            format!("{prefix}.self_attn.k_proj.weight"),
            format!("{prefix}.self_attn.v_proj.weight"),
            format!("{prefix}.self_attn.o_proj.weight"),
            format!("{prefix}.post_attention_layernorm.weight"),
            format!("{prefix}.mlp.gate_proj.weight"),
            format!("{prefix}.mlp.up_proj.weight"),
            format!("{prefix}.mlp.down_proj.weight"),
        ];
        tensors.push(Checkpoint::exact(&names[0], vec![HIDDEN], &mut state));
        tensors.push(Checkpoint::exact(
            &names[1],
            vec![HIDDEN, HIDDEN],
            &mut state,
        ));
        tensors.push(Checkpoint::exact(
            &names[2],
            vec![kv_dim, HIDDEN],
            &mut state,
        ));
        tensors.push(Checkpoint::exact(
            &names[3],
            vec![kv_dim, HIDDEN],
            &mut state,
        ));
        tensors.push(Checkpoint::exact(
            &names[4],
            vec![HIDDEN, HIDDEN],
            &mut state,
        ));
        tensors.push(Checkpoint::exact(&names[5], vec![HIDDEN], &mut state));
        tensors.push(Checkpoint::exact(&names[6], vec![FFN, HIDDEN], &mut state));
        tensors.push(Checkpoint::exact(&names[7], vec![FFN, HIDDEN], &mut state));
        tensors.push(Checkpoint::exact(&names[8], vec![HIDDEN, FFN], &mut state));
    }
    tensors.push(Checkpoint::exact(
        "model.norm.weight",
        vec![HIDDEN],
        &mut state,
    ));
    tensors
}

/// Write a `safetensors` file plus the config of a model root.
fn write_root(root: &Path, tensors: &[Checkpoint]) -> Result<()> {
    fs::create_dir_all(root).expect("the test directory is writable");
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
    while !header.len().is_multiple_of(8) {
        header.push(' ');
    }
    let mut file = Vec::with_capacity(8 + header.len() + data.len());
    file.extend_from_slice(&(header.len() as u64).to_le_bytes());
    file.extend_from_slice(header.as_bytes());
    file.extend_from_slice(&data);
    fs::write(root.join("model.safetensors"), file).expect("the weights are writable");
    let config = format!(
        "{{\"hidden_size\":{HIDDEN},\"num_hidden_layers\":{LAYERS},\
         \"num_attention_heads\":{HEADS},\"num_key_value_heads\":{KV_HEADS},\
         \"intermediate_size\":{FFN},\"rms_norm_eps\":1e-5,\"vocab_size\":{VOCAB},\
         \"tie_word_embeddings\":true,\"eos_token_id\":0,\
         \"rope_theta\":10000.0,\"rope_interleaved\":false}}"
    );
    fs::write(root.join("config.json"), config).expect("the config is writable");
    Ok(())
}

/// Greedy tokens of one prompt.
fn generate(model: &Model<'_>, prompt: &[u32], count: usize) -> Result<Vec<u32>> {
    let mut session = model.session(prompt.len() + count)?;
    for &token in prompt {
        session.forward(token)?;
    }
    let mut tokens = Vec::with_capacity(count);
    while tokens.len() < count {
        let token = session.sample()?;
        tokens.push(token);
        if tokens.len() == count {
            break;
        }
        session.forward(token)?;
    }
    Ok(tokens)
}

fn temporary(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "madrust-infer-{name}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn the_quantized_development_model_generates_the_same_tokens() {
    let float_root = temporary("gate-float");
    let quant_root = temporary("gate-quant");
    let tensors = tensors();
    write_root(&float_root, &tensors).expect("the floating point root is written");
    let report =
        quantize::checkpoint(&float_root, &quant_root, quantize::Width::Four)
        .expect("the checkpoint is quantized");
    assert!(report.quantized > 0, "the gate quantized no matrix");
    assert!(
        report.ratio() < 0.35,
        "the quantized checkpoint kept {} of the source bytes",
        report.ratio()
    );

    let config_bytes = fs::read(float_root.join("config.json")).expect("the config is readable");
    let config = Config::parse(&config_bytes).expect("the config parses");

    let float_file = ModelFile::open(
        float_root
            .join("model.safetensors")
            .to_str()
            .expect("the test path is UTF-8"),
    )
    .expect("the floating point checkpoint opens");
    let quant_file = ModelFile::open(
        quant_root
            .join("model.safetensors")
            .to_str()
            .expect("the test path is UTF-8"),
    )
    .expect("the quantized checkpoint opens");
    let float_model = Model::new(&float_file, &config).expect("the floating point model binds");
    let quant_model = Model::new(&quant_file, &config).expect("the quantized model binds");
    assert_eq!(float_model.weight_dtype(), "BF16");
    assert_eq!(quant_model.weight_dtype(), "Q4B32");

    let prompt = reference::PROMPT_IDS;
    let float_tokens = generate(&float_model, &prompt, COUNT).expect("the floating point run");
    let quant_tokens = generate(&quant_model, &prompt, COUNT).expect("the quantized run");
    assert_eq!(
        quant_tokens, float_tokens,
        "the quantized checkpoint changed the tokens"
    );

    // The logits of the two checkpoints must stay close, not just produce the
    // same argmax: a broken scale would still pass a token comparison on a
    // model whose answer is decided by a wide margin.
    let float_logits = {
        let mut session = float_model
            .session(prompt.len() + 1)
            .expect("the floating point session");
        for &token in &prompt {
            session.forward(token).expect("the floating point forward");
        }
        session.logits().to_vec()
    };
    let quant_logits = {
        let mut session = quant_model
            .session(prompt.len() + 1)
            .expect("the quantized session");
        for &token in &prompt {
            session.forward(token).expect("the quantized forward");
        }
        session.logits().to_vec()
    };
    let maximum = float_logits
        .iter()
        .zip(&quant_logits)
        .map(|(left, right)| (left - right).abs())
        .fold(0.0f32, f32::max);
    assert!(
        maximum < 1e-3,
        "the logits differ by {maximum}, the quantization is not faithful"
    );

    let _ = fs::remove_dir_all(&float_root);
    let _ = fs::remove_dir_all(&quant_root);
}

#[test]
fn the_reader_refuses_a_quantized_file_without_the_declaration() {
    let float_root = temporary("gate-bad-float");
    let quant_root = temporary("gate-bad-quant");
    let bad_root = temporary("gate-bad-header");
    write_root(&float_root, &tensors()).expect("the floating point root is written");
    quantize::checkpoint(&float_root, &quant_root, quantize::Width::Four)
        .expect("the checkpoint is quantized");

    // Rename the format inside the header. The header length and every offset
    // stay the same, so the file still parses; only the declaration disagrees
    // with the tensors, which is what the reader has to refuse.
    let mut bytes =
        fs::read(quant_root.join("model.safetensors")).expect("the checkpoint is readable");
    let header_len = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]) as usize;
    let from = b"madrust-q4b32";
    let to = b"madrust-q4b99";
    let header = &mut bytes[8..8 + header_len];
    let position = header
        .windows(from.len())
        .position(|window| window == from)
        .expect("the header declares the format");
    header[position..position + to.len()].copy_from_slice(to);
    fs::create_dir_all(&bad_root).expect("the test directory is writable");
    fs::write(bad_root.join("model.safetensors"), &bytes)
        .expect("the tampered checkpoint is writable");
    fs::copy(float_root.join("config.json"), bad_root.join("config.json"))
        .expect("the config is copied");

    let config_bytes = fs::read(float_root.join("config.json")).expect("the config is readable");
    let config = Config::parse(&config_bytes).expect("the config parses");
    let file = ModelFile::open(
        bad_root
            .join("model.safetensors")
            .to_str()
            .expect("the test path is UTF-8"),
    )
    .expect("the tampered file still maps");
    let error = match Model::new(&file, &config) {
        Ok(_) => panic!("the tampered file is accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("madrust-q4b32"),
        "the error does not name the format: {error}"
    );

    let _ = fs::remove_dir_all(&float_root);
    let _ = fs::remove_dir_all(&quant_root);
    let _ = fs::remove_dir_all(&bad_root);
}
