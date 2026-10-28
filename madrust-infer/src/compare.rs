// SPDX-License-Identifier: GPL-2.0-only
//! The floating point against quantized comparison, shared by the tool.
//!
//! The decoder needs the mapping path the runtime uses on Linux, so the module
//! lives with `model` and `mapping` behind the same target gate. It is a tool,
//! not a runtime path: the runtime never decodes a checkpoint twice.
//!
//! Two measurements come out of one run. Both checkpoints decode the prompt
//! greedily and independently, which gives the two token lists: the checkpoint
//! is ready only when they are the same. Then both decode the *floating point*
//! token sequence, so the logits of a step describe the same input, and the
//! largest and mean absolute difference over every logit of every step is the
//! fidelity of the weights themselves.

use std::io::Read;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mapping::open_readonly;
use crate::model::{Model, ModelFile};
use crate::reference;

/// What one comparison measured.
#[derive(Clone, Debug, Default)]
pub struct Diff {
    /// Steps the two checkpoints ran on the same input.
    pub steps: usize,
    /// Logits compared: steps times the vocabulary size.
    pub values: u64,
    /// Largest absolute difference of one logit.
    pub maximum: f32,
    /// Mean absolute difference of a logit.
    pub mean: f64,
}

impl Diff {
    fn add(&mut self, left: &[f32], right: &[f32]) -> Result<()> {
        if left.len() != right.len() {
            return Err(Error::new(format!(
                "compare: the two checkpoints have {} and {} logits",
                left.len(),
                right.len()
            )));
        }
        for (left, right) in left.iter().zip(right) {
            let difference = (left - right).abs();
            if difference > self.maximum {
                self.maximum = difference;
            }
            self.mean += difference as f64;
            self.values += 1;
        }
        self.steps += 1;
        Ok(())
    }

    /// The mean, computed once at the end.
    fn mean(&self) -> f64 {
        if self.values == 0 {
            0.0
        } else {
            self.mean / self.values as f64
        }
    }
}

/// Decode the published prompt with both checkpoints and report the difference.
pub fn run(float_path: &str, quant_path: &str, config_path: &str) -> Result<()> {
    let mut file = open_readonly(config_path)?;
    let mut config_bytes = Vec::new();
    file.read_to_end(&mut config_bytes)?;
    let config = Config::parse(&config_bytes)?;

    let float_file = ModelFile::open(float_path)?;
    let quant_file = ModelFile::open(quant_path)?;
    let float_model = Model::new(&float_file, &config)?;
    let quant_model = Model::new(&quant_file, &config)?;

    println!(
        "COMPARE models float={float_path} weights={} bytes={} quant={quant_path} weights={} bytes={}",
        float_model.weight_dtype(),
        float_file.file_len(),
        quant_model.weight_dtype(),
        quant_file.file_len(),
    );

    let prompt = reference::PROMPT_IDS;
    let count = reference::NEW_TOKENS.len();
    let capacity = prompt
        .len()
        .checked_add(count)
        .ok_or_else(|| Error::new("compare: context length overflow"))?;
    let eos = config.eos_token_id;

    // Independent greedy decode of both checkpoints.
    let mut float = float_model.session(capacity)?;
    let mut quant = quant_model.session(capacity)?;
    for &token in &prompt {
        float.forward(token)?;
        quant.forward(token)?;
    }
    let mut float_tokens = Vec::with_capacity(count);
    let mut quant_tokens = Vec::with_capacity(count);
    while float_tokens.len() < count {
        let next_float = float.sample()?;
        let next_quant = quant.sample()?;
        float_tokens.push(next_float);
        quant_tokens.push(next_quant);
        if float_tokens.len() == count || (next_float == eos && next_quant == eos) {
            break;
        }
        float.forward(next_float)?;
        quant.forward(next_quant)?;
    }

    // Both checkpoints now run the floating point sequence. The logits of a
    // step then describe the same input, so their difference is the fidelity of
    // the quantized weights and not the drift of two different conversations.
    let mut float = float_model.session(capacity)?;
    let mut quant = quant_model.session(capacity)?;
    let mut forced = Diff::default();
    for &token in &prompt {
        float.forward(token)?;
        quant.forward(token)?;
    }
    let mut prefill = Diff::default();
    prefill.add(float.logits(), quant.logits())?;
    for &token in &float_tokens {
        float.forward(token)?;
        quant.forward(token)?;
        forced.add(float.logits(), quant.logits())?;
    }

    println!("COMPARE tokens_float {}", join(&float_tokens));
    println!("COMPARE tokens_quant {}", join(&quant_tokens));
    println!(
        "COMPARE reference_float={} reference_quant={}",
        starts_with(&float_tokens, &reference::NEW_TOKENS),
        starts_with(&quant_tokens, &reference::NEW_TOKENS),
    );
    println!(
        "COMPARE logits steps={} values={} max={:.6} mean={:.9}",
        forced.steps,
        forced.values,
        forced.maximum,
        forced.mean()
    );
    println!(
        "COMPARE prefill max={:.6} mean={:.9}",
        prefill.maximum,
        prefill.mean()
    );
    let matches = float_tokens == quant_tokens;
    println!(
        "COMPARE result match={matches} tokens={} max_diff={:.6} mean_diff={:.9}",
        float_tokens.len(),
        forced.maximum,
        forced.mean()
    );
    if !matches {
        return Err(Error::new(
            "compare: the quantized checkpoint generates different tokens",
        ));
    }
    println!("COMPARE OK");
    Ok(())
}

/// `true` when `tokens` starts with `prefix`.
fn starts_with(tokens: &[u32], prefix: &[u32]) -> bool {
    tokens.len() >= prefix.len() && tokens[..prefix.len()] == prefix[..]
}

/// Join token ids for the log.
fn join(tokens: &[u32]) -> String {
    tokens
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}
