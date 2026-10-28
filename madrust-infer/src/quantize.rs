// SPDX-License-Identifier: GPL-2.0-only
//! Rewrite a `safetensors` checkpoint in the `Q4B32` block format.
//!
//! The function reads the tensor directory of `input/model.safetensors`,
//! quantizes every two dimensional matrix into blocks of thirty-two weights,
//! keeps the one dimensional norm weights in the type they were written with,
//! and writes the result as another `safetensors` file. It copies the
//! `config.json`, the vocabulary and the merges next to it when they are there,
//! so the output directory is a model root the runtime opens as it is.
//!
//! The format is described in `docs/QUANTIZATION.md`. The code is a host tool,
//! shared by the `madrust-infer-quantize` binary and by the gate that checks
//! that the quantized development model answers like the floating point one. It
//! has no dependency: the digest uses [`crate::sha256`].

use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::quant;
use crate::safetensors::{self, DType};
use crate::sha256;

/// Files the runtime reads from the model root, copied next to the weights.
const COMPANIONS: [&str; 4] = [
    "config.json",
    "vocab.json",
    "merges.txt",
    "tokenizer_config.json",
];

/// How many bits per weight the output holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Four bits in blocks of thirty-two: 4.5 bits per weight.
    Four,
    /// Eight bits in the same blocks: 8.5 bits per weight.
    Eight,
}

impl Width {
    /// Bits per weight, without the scale.
    pub fn bits(self) -> usize {
        match self {
            Self::Four => 4,
            Self::Eight => 8,
        }
    }

    /// Tensor type as the header spells it.
    pub fn dtype(self) -> &'static str {
        match self {
            Self::Four => "Q4B32",
            Self::Eight => "Q8B32",
        }
    }

    /// Format name as `__metadata__` spells it.
    pub fn format(self) -> &'static str {
        match self {
            Self::Four => quant::FORMAT_NAME,
            Self::Eight => quant::Q8_FORMAT_NAME,
        }
    }

    /// Bytes one row of `columns` weights occupies.
    fn row_bytes(self, columns: usize) -> Result<usize> {
        match self {
            Self::Four => quant::row_bytes(columns),
            Self::Eight => quant::q8_row_bytes(columns),
        }
    }

    /// Bytes one block occupies.
    fn block_bytes(self) -> usize {
        match self {
            Self::Four => quant::BLOCK_BYTES,
            Self::Eight => quant::Q8_BLOCK_BYTES,
        }
    }

    /// Pack one block of `BLOCK` values into `out`.
    fn quantize_block(self, values: &[f32], out: &mut [u8]) -> Result<()> {
        match self {
            Self::Four => quant::quantize_block(values, out),
            Self::Eight => quant::quantize_block_q8(values, out),
        }
    }
}

impl core::fmt::Display for Width {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}", self.bits())
    }
}

/// What one conversion wrote.
#[derive(Clone, Debug)]
pub struct Report {
    /// Path of the quantized checkpoint.
    pub model: String,
    /// Bytes of the quantized checkpoint.
    pub bytes: usize,
    /// Bytes of the source checkpoint.
    pub source_bytes: usize,
    /// Tensors in the checkpoint.
    pub tensors: usize,
    /// Tensors rewritten in the block format.
    pub quantized: usize,
    /// Tensors copied unchanged.
    pub kept: usize,
    /// Bits per weight of the quantized tensors.
    pub width: Width,
    /// SHA-256 of the quantized checkpoint.
    pub digest: [u8; 32],
}

impl Report {
    /// The digest in lower case hexadecimal.
    pub fn digest_hex(&self) -> String {
        sha256::hex(&self.digest)
    }

    /// Size of the quantized checkpoint against the source.
    pub fn ratio(&self) -> f64 {
        if self.source_bytes == 0 {
            0.0
        } else {
            self.bytes as f64 / self.source_bytes as f64
        }
    }
}

/// Quantize `input/model.safetensors` into `output/model.safetensors`.
pub fn checkpoint(input: &Path, output: &Path, width: Width) -> Result<Report> {
    let source = input.join("model.safetensors");
    let bytes = fs::read(&source)
        .map_err(|error| Error::new(format!("cannot read {}: {error}", source.display())))?;
    let (header_len, header, data) = split(&bytes)?;
    let directory = safetensors::parse(header, data.len())
        .map_err(|error| Error::new(format!("{}: {error}", source.display())))?;

    fs::create_dir_all(output)
        .map_err(|error| Error::new(format!("cannot create {}: {error}", output.display())))?;

    let mut out_data = Vec::new();
    let mut out_header = String::from("{\"__metadata__\":{\"format\":\"");
    out_header.push_str(width.format());
    out_header.push_str("\",\"block\":\"32\",\"scale\":\"");
    out_header.push_str(quant::SCALE_NAME);
    out_header.push_str("\"}");
    let mut quantized = 0usize;
    let mut kept = 0usize;

    for info in directory.tensors() {
        let name = directory.name(info);
        let body = data
            .get(info.start..info.end)
            .ok_or_else(|| Error::new(format!("{name}: data offsets are outside the file")))?;
        out_header.push_str(",\"");
        out_header.push_str(&escape(name));
        out_header.push_str("\":{\"dtype\":\"");
        let payload;
        if info.shape().len() == 2 {
            let values = decode_elements(body, info.dtype, name)?;
            let rows = info.shape()[0];
            let columns = info.shape()[1];
            let row_bytes = width.row_bytes(columns)?;
            let mut packed = vec![0u8; row_bytes * rows];
            for row in 0..rows {
                for block in 0..quant::blocks(columns) {
                    let value_start = row * columns + block * quant::BLOCK;
                    let byte_start = row * row_bytes + block * width.block_bytes();
                    width.quantize_block(
                        &values[value_start..value_start + quant::BLOCK],
                        &mut packed[byte_start..byte_start + width.block_bytes()],
                    )?;
                }
            }
            out_header.push_str(width.dtype());
            payload = packed;
            quantized += 1;
        } else {
            out_header.push_str(info.dtype.name());
            payload = body.to_vec();
            kept += 1;
        }
        out_header.push_str("\",\"shape\":[");
        for (index, dimension) in info.shape().iter().enumerate() {
            if index != 0 {
                out_header.push(',');
            }
            out_header.push_str(&dimension.to_string());
        }
        out_header.push_str("],\"data_offsets\":[");
        let offset = out_data.len();
        out_header.push_str(&offset.to_string());
        out_header.push(',');
        out_header.push_str(&(offset + payload.len()).to_string());
        out_header.push_str("]}");
        out_data.extend_from_slice(&payload);
    }
    out_header.push('}');
    // Safetensors pads the JSON so the data starts on an eight byte boundary.
    while !out_header.len().is_multiple_of(8) {
        out_header.push(' ');
    }

    let mut file = Vec::with_capacity(8 + out_header.len() + out_data.len());
    file.extend_from_slice(&(out_header.len() as u64).to_le_bytes());
    file.extend_from_slice(out_header.as_bytes());
    file.extend_from_slice(&out_data);

    let destination = output.join("model.safetensors");
    fs::write(&destination, &file)
        .map_err(|error| Error::new(format!("cannot write {}: {error}", destination.display())))?;
    for companion in COMPANIONS {
        let from = input.join(companion);
        if from.is_file() {
            let to = output.join(companion);
            fs::copy(&from, &to).map_err(|error| {
                Error::new(format!(
                    "cannot copy {} to {}: {error}",
                    from.display(),
                    to.display()
                ))
            })?;
        }
    }

    Ok(Report {
        model: destination.display().to_string(),
        bytes: file.len(),
        source_bytes: header_len + data.len(),
        tensors: directory.tensors().len(),
        quantized,
        kept,
        width,
        digest: sha256::digest(&file),
    })
}

/// Split a `safetensors` file into its header length, its header and its data.
fn split(bytes: &[u8]) -> Result<(usize, &[u8], &[u8])> {
    if bytes.len() < 8 {
        return Err(Error::new("the file is shorter than the header length"));
    }
    let header_len = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    let header_len = usize::try_from(header_len)
        .map_err(|_| Error::new("the header length does not fit this target"))?;
    let data_start = 8usize
        .checked_add(header_len)
        .ok_or_else(|| Error::new("the header length overflows"))?;
    if data_start > bytes.len() {
        return Err(Error::new("the header is longer than the file"));
    }
    Ok((header_len, &bytes[8..data_start], &bytes[data_start..]))
}

/// Decode a tensor body into `f32`, refusing a type the format cannot read.
fn decode_elements(body: &[u8], dtype: DType, name: &str) -> Result<Vec<f32>> {
    match dtype {
        DType::Bf16 => {
            if !body.len().is_multiple_of(2) {
                return Err(Error::new(format!(
                    "{name}: BF16 data is not a whole number of elements"
                )));
            }
            Ok(body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| f32::from_bits((u16::from_le_bytes(*pair) as u32) << 16))
                .collect())
        }
        DType::F32 => {
            if !body.len().is_multiple_of(4) {
                return Err(Error::new(format!(
                    "{name}: F32 data is not a whole number of elements"
                )));
            }
            Ok(body
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| f32::from_bits(u32::from_le_bytes(*word)))
                .collect())
        }
        DType::Q4B32 | DType::Q8B32 => Err(Error::new(format!(
            "{name}: the checkpoint is already quantized"
        ))),
        DType::Other => Err(Error::new(format!("{name}: unsupported source dtype"))),
    }
}

/// Escape a tensor name for the JSON header.
fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            character => out.push(character),
        }
    }
    out
}
