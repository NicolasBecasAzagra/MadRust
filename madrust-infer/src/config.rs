// SPDX-License-Identifier: GPL-2.0-only
//! The model configuration.
//!
//! `config.json` is the source of every dimension and of the generation
//! parameters a `safetensors` file cannot carry. The shape of the tensor
//! directory still has to agree with it, and the model code rejects a file that
//! disagrees, but the config is authoritative: a new checkpoint is a new config
//! and a new set of tensors, not a new binary.
//!
//! RoPE is accepted in both spellings: the flat `rope_theta` and
//! `rope_interleaved` fields this model ships, and the nested
//! `rope_parameters` object some transformers versions emit. The nested value
//! wins when both are present. `prompt_ids` and `expected_tokens` are optional:
//! the synthetic model writes its own prompt, and a reference run can carry the
//! golden tokens, so neither is compiled into the runtime.
//!
//! When the file is absent the reference model defaults are used. Every value
//! records whether it came from the file or from a default, so the serial log
//! can say where each one came from instead of hiding a fallback.
//!
//! The scanner is the flat object reader in [`crate::json`]. Unknown fields,
//! such as `architectures`, `torch_dtype` or `rope_scaling`, are skipped: they
//! do not change the arithmetic this runtime performs.

use crate::error::{Error, Result};
use crate::json::Scanner;

/// Default hidden size, from the reference SmolLM2-135M configuration.
pub const DEFAULT_HIDDEN_SIZE: usize = 576;
/// Default decoder layer count.
pub const DEFAULT_NUM_HIDDEN_LAYERS: usize = 30;
/// Default query head count.
pub const DEFAULT_NUM_ATTENTION_HEADS: usize = 9;
/// Default key value head count.
pub const DEFAULT_NUM_KEY_VALUE_HEADS: usize = 3;
/// Default feed forward width.
pub const DEFAULT_INTERMEDIATE_SIZE: usize = 1536;
/// Default RMSNorm epsilon.
pub const DEFAULT_RMS_NORM_EPS: f32 = 1e-5;
/// Default RoPE theta.
pub const DEFAULT_ROPE_THETA: f32 = 100_000.0;
/// Default vocabulary size.
pub const DEFAULT_VOCAB_SIZE: usize = 49_152;
/// Default end of sequence token, the reference model value.
pub const DEFAULT_EOS_TOKEN_ID: u32 = 0;

/// Where one configuration value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The value was present in `config.json`.
    Config,
    /// The file was absent or did not carry the field.
    Default,
}

impl Source {
    /// Short label for the serial log.
    pub fn label(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Default => "default",
        }
    }
}

/// One field the runtime reports in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// `hidden_size`.
    HiddenSize,
    /// `num_hidden_layers`.
    NumHiddenLayers,
    /// `num_attention_heads`.
    NumAttentionHeads,
    /// `num_key_value_heads`.
    NumKeyValueHeads,
    /// `intermediate_size`.
    IntermediateSize,
    /// `rms_norm_eps`.
    RmsNormEps,
    /// `rope_theta`.
    RopeTheta,
    /// `vocab_size`.
    VocabSize,
    /// `tie_word_embeddings`.
    TieWordEmbeddings,
    /// `eos_token_id`.
    EosTokenId,
    /// `rope_interleaved`.
    RopeInterleaved,
    /// `prompt_ids`.
    PromptIds,
    /// `expected_tokens`.
    ExpectedTokens,
}

impl Field {
    /// Every field, in log order.
    pub const ALL: [Self; 13] = [
        Self::HiddenSize,
        Self::NumHiddenLayers,
        Self::NumAttentionHeads,
        Self::NumKeyValueHeads,
        Self::IntermediateSize,
        Self::RmsNormEps,
        Self::RopeTheta,
        Self::VocabSize,
        Self::TieWordEmbeddings,
        Self::EosTokenId,
        Self::RopeInterleaved,
        Self::PromptIds,
        Self::ExpectedTokens,
    ];

    /// Name of the field as `config.json` spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::HiddenSize => "hidden_size",
            Self::NumHiddenLayers => "num_hidden_layers",
            Self::NumAttentionHeads => "num_attention_heads",
            Self::NumKeyValueHeads => "num_key_value_heads",
            Self::IntermediateSize => "intermediate_size",
            Self::RmsNormEps => "rms_norm_eps",
            Self::RopeTheta => "rope_theta",
            Self::VocabSize => "vocab_size",
            Self::TieWordEmbeddings => "tie_word_embeddings",
            Self::EosTokenId => "eos_token_id",
            Self::RopeInterleaved => "rope_interleaved",
            Self::PromptIds => "prompt_ids",
            Self::ExpectedTokens => "expected_tokens",
        }
    }
}

/// Every dimension and generation parameter the runtime reads.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Hidden size.
    pub hidden_size: usize,
    /// Decoder layers.
    pub num_hidden_layers: usize,
    /// Query heads.
    pub num_attention_heads: usize,
    /// Key value heads.
    pub num_key_value_heads: usize,
    /// Feed forward width.
    pub intermediate_size: usize,
    /// RMSNorm epsilon.
    pub rms_norm_eps: f32,
    /// RoPE theta.
    pub rope_theta: f32,
    /// Vocabulary size.
    pub vocab_size: usize,
    /// `true` when the output projection reuses the embedding matrix.
    pub tie_word_embeddings: bool,
    /// Token that ends generation.
    pub eos_token_id: u32,
    /// `true` for interleaved RoPE pairs, `false` for `(i, i + d/2)`.
    pub rope_interleaved: bool,
    /// Prompt ids, when the model config carries them.
    ///
    /// The synthetic model writes its own prompt here, so one binary can run
    /// both the small development model and the real checkpoint. When the file
    /// omits the field the runtime falls back to the baked reference prompt.
    pub prompt_ids: Option<Vec<u32>>,
    /// Tokens the reference implementation produces, when known.
    ///
    /// The runtime compares against them and fails a mismatch; the value comes
    /// from `reference.json` through the config, never from the binary.
    pub expected_tokens: Option<Vec<u32>>,
    sources: [Source; 13],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hidden_size: DEFAULT_HIDDEN_SIZE,
            num_hidden_layers: DEFAULT_NUM_HIDDEN_LAYERS,
            num_attention_heads: DEFAULT_NUM_ATTENTION_HEADS,
            num_key_value_heads: DEFAULT_NUM_KEY_VALUE_HEADS,
            intermediate_size: DEFAULT_INTERMEDIATE_SIZE,
            rms_norm_eps: DEFAULT_RMS_NORM_EPS,
            rope_theta: DEFAULT_ROPE_THETA,
            vocab_size: DEFAULT_VOCAB_SIZE,
            tie_word_embeddings: true,
            eos_token_id: DEFAULT_EOS_TOKEN_ID,
            rope_interleaved: false,
            prompt_ids: None,
            expected_tokens: None,
            sources: [Source::Default; 13],
        }
    }
}

impl Config {
    /// The reference model defaults, for a boot without `config.json`.
    pub fn defaults() -> Self {
        Self::default()
    }

    /// Parse the config object. Absent fields keep their default value.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut config = Self::defaults();
        let mut scanner = Scanner::new(bytes);
        let mut flat_theta = None;
        let mut flat_interleaved = None;
        let mut nested_theta = None;
        let mut nested_interleaved = None;

        scanner.skip_whitespace();
        scanner.expect(b'{')?;
        scanner.skip_whitespace();
        if scanner.peek() == Some(b'}') {
            scanner.expect(b'}')?;
            return config.validate();
        }

        loop {
            scanner.skip_whitespace();
            let key = scanner.parse_string()?;
            scanner.skip_whitespace();
            scanner.expect(b':')?;
            scanner.skip_whitespace();
            match key.as_str() {
                "hidden_size" => {
                    config.hidden_size = scanner.parse_usize()?;
                    config.set(Field::HiddenSize);
                }
                "num_hidden_layers" => {
                    config.num_hidden_layers = scanner.parse_usize()?;
                    config.set(Field::NumHiddenLayers);
                }
                "num_attention_heads" => {
                    config.num_attention_heads = scanner.parse_usize()?;
                    config.set(Field::NumAttentionHeads);
                }
                "num_key_value_heads" => {
                    config.num_key_value_heads = scanner.parse_usize()?;
                    config.set(Field::NumKeyValueHeads);
                }
                "intermediate_size" => {
                    config.intermediate_size = scanner.parse_usize()?;
                    config.set(Field::IntermediateSize);
                }
                "rms_norm_eps" => {
                    config.rms_norm_eps = scanner.parse_number()? as f32;
                    config.set(Field::RmsNormEps);
                }
                "rope_theta" => flat_theta = Some(scanner.parse_number()? as f32),
                "vocab_size" => {
                    config.vocab_size = scanner.parse_usize()?;
                    config.set(Field::VocabSize);
                }
                "tie_word_embeddings" => {
                    config.tie_word_embeddings = scanner.parse_bool()?;
                    config.set(Field::TieWordEmbeddings);
                }
                "eos_token_id" => {
                    config.eos_token_id = u32::try_from(scanner.parse_usize()?)
                        .map_err(|_| Error::new("config: eos_token_id does not fit u32"))?;
                    config.set(Field::EosTokenId);
                }
                "rope_interleaved" => flat_interleaved = Some(scanner.parse_bool()?),
                "rope_parameters" => {
                    parse_rope_parameters(
                        &mut scanner,
                        &mut nested_theta,
                        &mut nested_interleaved,
                    )?;
                }
                "prompt_ids" => {
                    config.prompt_ids = Some(parse_id_array(&mut scanner)?);
                    config.set(Field::PromptIds);
                }
                "expected_tokens" => {
                    config.expected_tokens = Some(parse_id_array(&mut scanner)?);
                    config.set(Field::ExpectedTokens);
                }
                _ => scanner.skip_value()?,
            }
            scanner.skip_whitespace();
            match scanner.bump()? {
                b',' => continue,
                b'}' => break,
                _ => return Err(Error::new("config: expected ',' or '}'")),
            }
        }
        if !scanner.at_end() {
            return Err(Error::new("config: trailing bytes after the object"));
        }
        // A nested `rope_parameters` object is the current spelling; the flat
        // fields are the older one. The nested value wins when both are present.
        if let Some(theta) = nested_theta.or(flat_theta) {
            config.rope_theta = theta;
            config.set(Field::RopeTheta);
        }
        if let Some(interleaved) = nested_interleaved.or(flat_interleaved) {
            config.rope_interleaved = interleaved;
            config.set(Field::RopeInterleaved);
        }
        config.validate()
    }

    /// Where one field came from.
    pub fn source(&self, field: Field) -> Source {
        self.sources[field as usize]
    }

    /// A compact `name=value(source)` list for the serial log.
    pub fn provenance(&self) -> String {
        let mut out = String::new();
        for field in Field::ALL {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(field.name());
            out.push('=');
            out.push_str(&self.value(field));
            out.push('(');
            out.push_str(self.source(field).label());
            out.push(')');
        }
        out
    }

    /// Format one field value.
    fn value(&self, field: Field) -> String {
        match field {
            Field::HiddenSize => self.hidden_size.to_string(),
            Field::NumHiddenLayers => self.num_hidden_layers.to_string(),
            Field::NumAttentionHeads => self.num_attention_heads.to_string(),
            Field::NumKeyValueHeads => self.num_key_value_heads.to_string(),
            Field::IntermediateSize => self.intermediate_size.to_string(),
            Field::RmsNormEps => self.rms_norm_eps.to_string(),
            Field::RopeTheta => self.rope_theta.to_string(),
            Field::VocabSize => self.vocab_size.to_string(),
            Field::TieWordEmbeddings => self.tie_word_embeddings.to_string(),
            Field::EosTokenId => self.eos_token_id.to_string(),
            Field::RopeInterleaved => self.rope_interleaved.to_string(),
            Field::PromptIds => format_ids(self.prompt_ids.as_deref()),
            Field::ExpectedTokens => format_ids(self.expected_tokens.as_deref()),
        }
    }

    fn set(&mut self, field: Field) {
        self.sources[field as usize] = Source::Config;
    }

    /// Reject values that cannot describe a model.
    fn validate(self) -> Result<Self> {
        if self.hidden_size == 0
            || self.num_hidden_layers == 0
            || self.num_attention_heads == 0
            || self.num_key_value_heads == 0
            || self.intermediate_size == 0
            || self.vocab_size == 0
        {
            return Err(Error::new("config: every dimension must be positive"));
        }
        if !self.hidden_size.is_multiple_of(self.num_attention_heads) {
            return Err(Error::new(
                "config: hidden_size is not divisible by num_attention_heads",
            ));
        }
        if !self
            .num_attention_heads
            .is_multiple_of(self.num_key_value_heads)
        {
            return Err(Error::new(
                "config: num_key_value_heads does not divide num_attention_heads",
            ));
        }
        if !self.rms_norm_eps.is_finite() || self.rms_norm_eps <= 0.0 {
            return Err(Error::new("config: rms_norm_eps must be positive"));
        }
        if !self.rope_theta.is_finite() || self.rope_theta <= 0.0 {
            return Err(Error::new("config: rope_theta must be positive"));
        }
        if usize::try_from(self.eos_token_id)
            .ok()
            .is_none_or(|id| id >= self.vocab_size)
        {
            return Err(Error::new("config: eos_token_id is outside the vocabulary"));
        }
        Ok(self)
    }
}

/// Parse one array of token ids.
fn parse_id_array(scanner: &mut Scanner<'_>) -> Result<Vec<u32>> {
    let values = scanner.parse_usize_array()?;
    let mut ids = Vec::with_capacity(values.len());
    for value in values {
        ids.push(
            u32::try_from(value).map_err(|_| Error::new("config: token id does not fit u32"))?,
        );
    }
    Ok(ids)
}

/// Read the nested `rope_parameters` object.
fn parse_rope_parameters(
    scanner: &mut Scanner<'_>,
    theta: &mut Option<f32>,
    interleaved: &mut Option<bool>,
) -> Result<()> {
    scanner.skip_whitespace();
    if scanner.peek() != Some(b'{') {
        return scanner.skip_value();
    }
    scanner.expect(b'{')?;
    scanner.skip_whitespace();
    if scanner.peek() == Some(b'}') {
        scanner.expect(b'}')?;
        return Ok(());
    }
    loop {
        scanner.skip_whitespace();
        let key = scanner.parse_string()?;
        scanner.skip_whitespace();
        scanner.expect(b':')?;
        scanner.skip_whitespace();
        match key.as_str() {
            "rope_theta" => *theta = Some(scanner.parse_number()? as f32),
            "rope_interleaved" => *interleaved = Some(scanner.parse_bool()?),
            _ => scanner.skip_value()?,
        }
        scanner.skip_whitespace();
        match scanner.bump()? {
            b',' => continue,
            b'}' => return Ok(()),
            _ => return Err(Error::new("config: expected ',' or '}' in rope_parameters")),
        }
    }
}

/// Format an optional id array for the log.
fn format_ids(ids: Option<&[u32]>) -> String {
    match ids {
        Some(ids) => ids.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
        None => "none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, Field, Source, DEFAULT_HIDDEN_SIZE, DEFAULT_NUM_HIDDEN_LAYERS,
        DEFAULT_RMS_NORM_EPS, DEFAULT_ROPE_THETA, DEFAULT_VOCAB_SIZE,
    };

    #[test]
    fn reads_every_field_and_skips_unknown_ones() {
        let config = Config::parse(
            br#"{"hidden_size":576,"num_hidden_layers":30,
                 "num_attention_heads":9,"num_key_value_heads":3,
                 "intermediate_size":1536,"rms_norm_eps":1e-5,
                 "rope_theta":100000.0,"vocab_size":49152,
                 "tie_word_embeddings":true,"eos_token_id":0,
                 "rope_interleaved":false,
                 "prompt_ids":[504,3575,282,4649,314],
                 "expected_tokens":[260,3575,282,260,1798,30,198,198],
                 "architectures":["LlamaForCausalLM"],"torch_dtype":"bfloat16"}"#,
        )
        .unwrap();
        assert_eq!(config.hidden_size, 576);
        assert_eq!(config.num_hidden_layers, 30);
        assert_eq!(config.num_attention_heads, 9);
        assert_eq!(config.num_key_value_heads, 3);
        assert_eq!(config.intermediate_size, 1536);
        assert_eq!(config.rope_theta, 100_000.0);
        assert_eq!(config.vocab_size, 49_152);
        assert!(config.tie_word_embeddings);
        assert_eq!(config.eos_token_id, 0);
        assert!(!config.rope_interleaved);
        assert_eq!(
            config.prompt_ids.as_deref(),
            Some(&[504, 3575, 282, 4649, 314][..])
        );
        assert_eq!(
            config.expected_tokens.as_deref(),
            Some(&[260, 3575, 282, 260, 1798, 30, 198, 198][..])
        );
        for field in Field::ALL {
            assert_eq!(config.source(field), Source::Config);
        }
    }

    #[test]
    fn uses_the_reference_defaults_when_the_file_is_missing() {
        let config = Config::defaults();
        assert_eq!(config.hidden_size, DEFAULT_HIDDEN_SIZE);
        assert_eq!(config.num_hidden_layers, DEFAULT_NUM_HIDDEN_LAYERS);
        assert_eq!(config.rms_norm_eps, DEFAULT_RMS_NORM_EPS);
        assert_eq!(config.rope_theta, DEFAULT_ROPE_THETA);
        assert_eq!(config.vocab_size, DEFAULT_VOCAB_SIZE);
        for field in Field::ALL {
            assert_eq!(config.source(field), Source::Default);
        }
    }

    #[test]
    fn keeps_the_default_for_a_field_the_file_omits() {
        let config = Config::parse(br#"{"hidden_size":576}"#).unwrap();
        assert_eq!(config.hidden_size, 576);
        assert_eq!(config.source(Field::HiddenSize), Source::Config);
        assert_eq!(config.num_hidden_layers, DEFAULT_NUM_HIDDEN_LAYERS);
        assert_eq!(config.source(Field::NumHiddenLayers), Source::Default);
        let provenance = config.provenance();
        assert!(provenance.contains("hidden_size=576(config)"));
        assert!(provenance.contains("num_hidden_layers=30(default)"));
    }

    #[test]
    fn reads_the_nested_rope_parameters() {
        let config = Config::parse(
            br#"{"hidden_size":576,"num_attention_heads":9,
                 "rope_parameters":{"rope_type":"default","rope_theta":100000.0,
                                    "rope_interleaved":false}}"#,
        )
        .unwrap();
        assert_eq!(config.rope_theta, 100_000.0);
        assert!(!config.rope_interleaved);
        assert_eq!(config.source(Field::RopeTheta), Source::Config);
        assert_eq!(config.source(Field::RopeInterleaved), Source::Config);
    }

    #[test]
    fn nested_rope_parameters_win_over_flat_fields() {
        let config = Config::parse(
            br#"{"hidden_size":576,"num_attention_heads":9,
                 "rope_theta":1.0,"rope_interleaved":true,
                 "rope_parameters":{"rope_theta":100000.0,"rope_interleaved":false}}"#,
        )
        .unwrap();
        assert_eq!(config.rope_theta, 100_000.0);
        assert!(!config.rope_interleaved);
    }

    #[test]
    fn reads_prompt_and_expected_tokens_from_the_config() {
        let config = Config::parse(
            br#"{"hidden_size":576,"num_attention_heads":9,
                 "prompt_ids":[504,3575,282,4649,314],
                 "expected_tokens":[260,3575,282,260,1798,30,198,198]}"#,
        )
        .unwrap();
        assert_eq!(
            config.prompt_ids.as_deref(),
            Some(&[504, 3575, 282, 4649, 314][..])
        );
        assert_eq!(config.source(Field::PromptIds), Source::Config);
        assert_eq!(config.source(Field::ExpectedTokens), Source::Config);
    }

    #[test]
    fn accepts_an_empty_object() {
        let config = Config::parse(b"{}").unwrap();
        assert_eq!(config, Config::defaults());
    }

    #[test]
    fn rejects_a_head_count_that_does_not_divide_the_hidden_size() {
        assert!(Config::parse(br#"{"hidden_size":10,"num_attention_heads":3}"#).is_err());
    }

    #[test]
    fn rejects_an_eos_outside_the_vocabulary() {
        assert!(Config::parse(br#"{"vocab_size":10,"eos_token_id":10}"#).is_err());
    }
}
