// SPDX-License-Identifier: GPL-2.0-only
//! The Llama-style decoder over a mapped `safetensors` file.
//!
//! The model keeps the tensor directory and a borrowed view of every matrix it
//! needs. A matrix is `[out_features, in_features]`, so one output element is a
//! dot product of a row with the input vector. The projections have no bias and
//! the embedding matrix is reused as the output projection when the checkpoint
//! has no separate `lm_head.weight`.
//!
//! `bfloat16` is converted at read time with
//! `f32::from_bits((bits as u32) << 16)`. The conversion is exact: a `bfloat16`
//! is the top half of an `f32`, so the shifted bits reproduce the original
//! `f32` with the low sixteen bits zero and no rounding step.
//!
//! The matrix vector products go through [`crate::matvec`]. A tensor that lies
//! inside the mapped prefix is handed to the blocked kernel as one contiguous
//! `bfloat16` range; the last at most 4095 bytes of the file live in a small
//! buffer instead of the mapping, so the few rows that reach into that tail
//! still use the element accessor. No weight is copied. The conversion to
//! `f32` happens in registers, which halves the weight traffic against a cached
//! `f32` copy (269 MB against 538 MB for the reference model) at the cost of
//! one shift and widening per weight, per token.

use std::time::Instant;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mapping::Mapping;
use crate::math::{argmax, rms_norm, rope, silu, softmax};
use crate::matvec;
use crate::safetensors::{self, DType, TensorInfo};

/// The dimensions the runtime read from the config and derived for the model.
#[derive(Clone, Copy, Debug)]
pub struct Dims {
    /// Hidden size.
    pub hidden: usize,
    /// Decoder layers.
    pub layers: usize,
    /// Query heads.
    pub q_heads: usize,
    /// Key value heads.
    pub kv_heads: usize,
    /// Elements per head.
    pub head_dim: usize,
    /// Feed forward width.
    pub ffn: usize,
    /// Vocabulary size.
    pub vocab: usize,
    /// RMSNorm epsilon.
    pub eps: f32,
    /// RoPE theta.
    pub rope_theta: f32,
    /// `true` when the RoPE pairs are interleaved.
    pub rope_interleaved: bool,
    /// `true` when the output projection reuses the embedding matrix.
    pub tie_word_embeddings: bool,
    /// Token that ends generation.
    pub eos_token_id: u32,
}

impl Dims {
    /// Group size of grouped query attention.
    pub fn group_size(&self) -> usize {
        self.q_heads / self.kv_heads
    }
}

/// A mapped model file and its tensor directory.
pub struct ModelFile {
    mapping: Mapping,
    header: safetensors::Header,
    data_start: usize,
}

impl ModelFile {
    /// Open the file, read the header and map the data.
    pub fn open(path: &str) -> Result<Self> {
        let mapping = Mapping::open(path)?;
        let file_len = mapping.file_len();
        if file_len < 8 {
            return Err(Error::new("model: file is shorter than the header length"));
        }
        let length_bytes = mapping.read_bytes(0, 8)?;
        let header_len = u64::from_le_bytes([
            length_bytes[0],
            length_bytes[1],
            length_bytes[2],
            length_bytes[3],
            length_bytes[4],
            length_bytes[5],
            length_bytes[6],
            length_bytes[7],
        ]);
        let header_len = usize::try_from(header_len)
            .map_err(|_| Error::new("model: header length does not fit this target"))?;
        let data_start = 8usize
            .checked_add(header_len)
            .ok_or_else(|| Error::new("model: header length overflow"))?;
        if data_start > file_len {
            return Err(Error::new("model: header is longer than the file"));
        }
        let header_bytes = mapping.read_bytes(8, header_len)?;
        let header = safetensors::parse(&header_bytes, file_len - data_start)?;
        Ok(Self {
            mapping,
            header,
            data_start,
        })
    }

    /// Number of tensors in the directory.
    pub fn tensor_count(&self) -> usize {
        self.header.tensors().len()
    }

    /// Bytes of the model that are mapped, always a multiple of the page size.
    pub fn mapped_len(&self) -> usize {
        self.mapping.mapped_len()
    }

    /// `true` when the kernel accepted the large page advice on the mapping.
    pub fn huge_pages_requested(&self) -> bool {
        self.mapping.huge_pages_requested()
    }

    /// Ask the kernel to make the mapped weights resident and keep them there.
    ///
    /// Returns the number of pinned bytes on success. The final partial page
    /// lives in a small buffer rather than the mapping, so it is not part of
    /// this range; every mapped weight byte is.
    pub fn pin(&self) -> core::result::Result<usize, i64> {
        self.mapping.pin()?;
        Ok(self.mapped_len())
    }

    /// Borrow one tensor, validating its dtype, shape and byte range.
    fn tensor(&self, name: &str) -> Result<Tensor<'_>> {
        let info = self
            .header
            .find(name)
            .ok_or_else(|| Error::new(format!("model: missing tensor {name}")))?;
        let tensor_name = self.header.name(info);
        Tensor::new(info, tensor_name, &self.mapping, self.data_start)
    }
}

/// One tensor viewed through the mapping.
#[derive(Clone, Copy)]
struct Tensor<'a> {
    name: &'a str,
    info: &'a TensorInfo,
    mapping: &'a Mapping,
    file_offset: usize,
    element_size: usize,
}

impl<'a> Tensor<'a> {
    fn new(
        info: &'a TensorInfo,
        name: &'a str,
        mapping: &'a Mapping,
        data_start: usize,
    ) -> Result<Self> {
        let element_size = info
            .dtype
            .element_size()
            .ok_or_else(|| Error::new(format!("model: tensor {name} has unsupported dtype")))?;
        let elements = info.element_count();
        let bytes = elements
            .checked_mul(element_size)
            .ok_or_else(|| Error::new(format!("model: tensor {name} is too large")))?;
        if bytes != info.byte_len() {
            return Err(Error::new(format!(
                "model: tensor {name} has a byte length that disagrees with its shape"
            )));
        }
        let file_offset = data_start
            .checked_add(info.start)
            .ok_or_else(|| Error::new("model: tensor offset overflow"))?;
        if file_offset + bytes > mapping.file_len() {
            return Err(Error::new(format!(
                "model: tensor {name} reaches past the file"
            )));
        }
        Ok(Self {
            name,
            info,
            mapping,
            file_offset,
            element_size,
        })
    }

    fn shape(&self) -> &[usize] {
        self.info.shape()
    }

    fn dtype(&self) -> DType {
        self.info.dtype
    }

    /// Read one element as `f32`.
    fn value(&self, index: usize) -> f32 {
        let offset = self.file_offset + index * self.element_size;
        match self.dtype() {
            DType::Bf16 => f32::from_bits((self.mapping.u16_at(offset) as u32) << 16),
            DType::F32 => f32::from_bits(self.mapping.u32_at(offset)),
            DType::Other => 0.0,
        }
    }

    /// The rows of a `bfloat16` matrix that are contiguous in the mapping.
    ///
    /// The returned bytes hold whole rows, so a caller can hand them to the
    /// blocked kernel. `None` means the matrix is not `bfloat16` or starts
    /// outside the mapping.
    fn contiguous_bf16(&self) -> Option<(&[u8], usize)> {
        if self.dtype() != DType::Bf16 || self.shape().len() != 2 {
            return None;
        }
        let rows = self.shape()[0];
        let columns = self.shape()[1];
        let row_bytes = columns.checked_mul(2)?;
        if row_bytes == 0 {
            return None;
        }
        let available = self.mapping.bytes_from(self.file_offset);
        let mapped_rows = (available.len() / row_bytes).min(rows);
        if mapped_rows == 0 {
            return None;
        }
        let length = mapped_rows.checked_mul(row_bytes)?;
        Some((available.get(..length)?, mapped_rows))
    }

    /// Dot one row of a matrix with `vector`.
    fn dot(&self, row: usize, vector: &[f32]) -> f32 {
        let columns = self.shape().last().copied().unwrap_or(0);
        if vector.len() != columns {
            return 0.0;
        }
        let base = row * columns;
        let start = self.file_offset + base * self.element_size;
        let in_prefix = if start < self.mapping.mapped_len() {
            ((self.mapping.mapped_len() - start) / self.element_size).min(columns)
        } else {
            0
        };
        let prefix = self.mapping.prefix();
        let mut sum = 0.0f32;
        match self.dtype() {
            DType::Bf16 => {
                for (index, &input) in vector.iter().enumerate().take(in_prefix) {
                    let offset = start + index * 2;
                    let bits = u16::from_le_bytes([prefix[offset], prefix[offset + 1]]);
                    sum += f32::from_bits((bits as u32) << 16) * input;
                }
            }
            DType::F32 => {
                for (index, &input) in vector.iter().enumerate().take(in_prefix) {
                    let offset = start + index * 4;
                    let bits = u32::from_le_bytes([
                        prefix[offset],
                        prefix[offset + 1],
                        prefix[offset + 2],
                        prefix[offset + 3],
                    ]);
                    sum += f32::from_bits(bits) * input;
                }
            }
            DType::Other => return 0.0,
        }
        for (index, &input) in vector.iter().enumerate().skip(in_prefix) {
            sum += self.value(base + index) * input;
        }
        sum
    }

    /// Copy one row of a matrix into `out`.
    fn read_row(&self, row: usize, out: &mut [f32]) -> Result<()> {
        let columns = self.shape().last().copied().unwrap_or(0);
        if out.len() != columns || row >= self.shape()[0] {
            return Err(Error::new(format!(
                "model: cannot read row {row} of tensor {}",
                self.name
            )));
        }
        let base = row * columns;
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.value(base + index);
        }
        Ok(())
    }

    /// The one dimensional weight of a norm.
    fn read_vector(&self, out: &mut [f32]) -> Result<()> {
        if self.shape().len() != 1 || self.shape()[0] != out.len() {
            return Err(Error::new(format!(
                "model: tensor {} is not a vector of the expected length",
                self.name
            )));
        }
        for (index, slot) in out.iter_mut().enumerate() {
            *slot = self.value(index);
        }
        Ok(())
    }
}

/// One decoder layer.
struct Layer<'a> {
    input_norm: Tensor<'a>,
    q_proj: Tensor<'a>,
    k_proj: Tensor<'a>,
    v_proj: Tensor<'a>,
    o_proj: Tensor<'a>,
    post_norm: Tensor<'a>,
    gate_proj: Tensor<'a>,
    up_proj: Tensor<'a>,
    down_proj: Tensor<'a>,
}

/// The decoder weights and the dimensions they imply.
pub struct Model<'a> {
    dims: Dims,
    embed: Tensor<'a>,
    layers: Vec<Layer<'a>>,
    final_norm: Tensor<'a>,
    output: Tensor<'a>,
    tensor_count: usize,
}

impl<'a> Model<'a> {
    /// Bind the tensors of `file` to the dimensions in `config`.
    ///
    /// The config is authoritative. Every tensor shape is checked against it
    /// and a disagreement is an error: a checkpoint and its config are shipped
    /// together, so a mismatch would otherwise silently compute a different
    /// model.
    pub fn new(file: &'a ModelFile, config: &Config) -> Result<Self> {
        let hidden = config.hidden_size;
        let layer_count = config.num_hidden_layers;
        let q_heads = config.num_attention_heads;
        let kv_heads = config.num_key_value_heads;
        let ffn = config.intermediate_size;
        let vocab = config.vocab_size;
        let head_dim = hidden / q_heads;
        let kv_dim = kv_heads
            .checked_mul(head_dim)
            .ok_or_else(|| Error::new("model: key value width overflow"))?;

        let embed = file.tensor("model.embed_tokens.weight")?;
        require_matrix(&embed, vocab, hidden, "model.embed_tokens.weight")?;

        let actual_layers = count_layers(file)?;
        if actual_layers != layer_count {
            return Err(Error::new(format!(
                "model: config says {layer_count} layers, the tensors carry {actual_layers}"
            )));
        }

        let mut layers = Vec::with_capacity(layer_count);
        for index in 0..layer_count {
            let input_norm = layer_tensor(file, index, "input_layernorm.weight")?;
            let q_proj = layer_tensor(file, index, "self_attn.q_proj.weight")?;
            let k_proj = layer_tensor(file, index, "self_attn.k_proj.weight")?;
            let v_proj = layer_tensor(file, index, "self_attn.v_proj.weight")?;
            let o_proj = layer_tensor(file, index, "self_attn.o_proj.weight")?;
            let post_norm = layer_tensor(file, index, "post_attention_layernorm.weight")?;
            let gate_proj = layer_tensor(file, index, "mlp.gate_proj.weight")?;
            let up_proj = layer_tensor(file, index, "mlp.up_proj.weight")?;
            let down_proj = layer_tensor(file, index, "mlp.down_proj.weight")?;

            require_matrix(&q_proj, hidden, hidden, "q_proj")?;
            require_matrix(&k_proj, kv_dim, hidden, "k_proj")?;
            require_matrix(&v_proj, kv_dim, hidden, "v_proj")?;
            require_matrix(&o_proj, hidden, hidden, "o_proj")?;
            require_matrix(&gate_proj, ffn, hidden, "gate_proj")?;
            require_matrix(&up_proj, ffn, hidden, "up_proj")?;
            require_matrix(&down_proj, hidden, ffn, "down_proj")?;
            require_vector(&input_norm, hidden, "input_layernorm.weight")?;
            require_vector(&post_norm, hidden, "post_attention_layernorm.weight")?;

            layers.push(Layer {
                input_norm,
                q_proj,
                k_proj,
                v_proj,
                o_proj,
                post_norm,
                gate_proj,
                up_proj,
                down_proj,
            });
        }

        let final_norm = file.tensor("model.norm.weight")?;
        require_vector(&final_norm, hidden, "model.norm.weight")?;

        // The config says whether the output projection is a separate matrix.
        // When it is tied, the embedding matrix is reused and a checkpoint
        // that also carries `lm_head.weight` still uses the tied path.
        let output = if config.tie_word_embeddings {
            embed
        } else {
            let head = file.tensor("lm_head.weight")?;
            require_matrix(&head, vocab, hidden, "lm_head.weight")?;
            head
        };

        Ok(Self {
            dims: Dims {
                hidden,
                layers: layer_count,
                q_heads,
                kv_heads,
                head_dim,
                ffn,
                vocab,
                eps: config.rms_norm_eps,
                rope_theta: config.rope_theta,
                rope_interleaved: config.rope_interleaved,
                tie_word_embeddings: config.tie_word_embeddings,
                eos_token_id: config.eos_token_id,
            },
            embed,
            layers,
            final_norm,
            output,
            tensor_count: file.tensor_count(),
        })
    }

    /// The dimensions read from the model.
    pub fn dims(&self) -> Dims {
        self.dims
    }

    /// Number of tensors the file carries.
    pub fn tensor_count(&self) -> usize {
        self.tensor_count
    }

    /// Name of the tensor element type the model was written with.
    pub fn weight_dtype(&self) -> &'static str {
        self.embed.dtype().name()
    }

    /// Start a generation session with room for `capacity` positions.
    ///
    /// The key value cache is one flat allocation per tensor instead of one
    /// vector per layer. A real model has thirty layers, and two vectors per
    /// layer would exceed the kernel's user mapping table before the first
    /// token; two allocations stay constant in the layer count.
    pub fn session(&self, capacity: usize) -> Result<Session<'_>> {
        if capacity == 0 {
            return Err(Error::new("model: the context capacity is zero"));
        }
        let dims = self.dims;
        let kv_dim = dims
            .kv_heads
            .checked_mul(dims.head_dim)
            .ok_or_else(|| Error::new("model: key value width overflow"))?;
        let cache_len = dims
            .layers
            .checked_mul(capacity)
            .and_then(|layers| layers.checked_mul(kv_dim))
            .ok_or_else(|| Error::new("model: key value cache is too large"))?;
        Ok(Session {
            model: self,
            capacity,
            position: 0,
            keys: vec![0.0f32; cache_len],
            values: vec![0.0f32; cache_len],
            x: vec![0.0f32; dims.hidden],
            norm: vec![0.0f32; dims.hidden],
            norm_weight: vec![0.0f32; dims.hidden],
            q: vec![0.0f32; dims.q_heads * dims.head_dim],
            k: vec![0.0f32; kv_dim],
            v: vec![0.0f32; kv_dim],
            attn: vec![0.0f32; dims.q_heads * dims.head_dim],
            projected: vec![0.0f32; dims.hidden],
            gate: vec![0.0f32; dims.ffn],
            up: vec![0.0f32; dims.ffn],
            scores: vec![0.0f32; capacity],
            logits: vec![0.0f32; dims.vocab],
            matmul_macs: 0,
            matmul_nanos: 0,
        })
    }
}

/// One generation session with its key value cache and scratch buffers.
///
/// Every buffer is allocated once, when the session starts. The forward pass
/// does not allocate, so a kernel that cannot grow a large vector through
/// `realloc` still runs long sequences.
pub struct Session<'a> {
    model: &'a Model<'a>,
    capacity: usize,
    position: usize,
    keys: Vec<f32>,
    values: Vec<f32>,
    x: Vec<f32>,
    norm: Vec<f32>,
    norm_weight: Vec<f32>,
    q: Vec<f32>,
    k: Vec<f32>,
    v: Vec<f32>,
    attn: Vec<f32>,
    projected: Vec<f32>,
    gate: Vec<f32>,
    up: Vec<f32>,
    scores: Vec<f32>,
    logits: Vec<f32>,
    matmul_macs: u64,
    matmul_nanos: u128,
}

/// Time and count the matrix vector products of one forward pass.
///
/// The counters are plain fields because the runtime has one thread and one
/// session. They measure only the multiply accumulates: the norms, the
/// attention and the activation are outside the clock, so the reported rate can
/// be compared with the isolated benchmark.
struct MatmulClock {
    macs: u64,
    nanos: u128,
}

impl MatmulClock {
    fn new() -> Self {
        Self { macs: 0, nanos: 0 }
    }

    fn run<F>(&mut self, work: F) -> Result<()>
    where
        F: FnOnce() -> Result<u64>,
    {
        let started = Instant::now();
        let macs = work()?;
        self.nanos = self.nanos.saturating_add(started.elapsed().as_nanos());
        self.macs = self.macs.saturating_add(macs);
        Ok(())
    }
}

impl Session<'_> {
    /// Multiply accumulates the forward passes performed.
    pub fn matmul_macs(&self) -> u64 {
        self.matmul_macs
    }

    /// Nanoseconds spent inside the matrix vector products.
    pub fn matmul_nanos(&self) -> u128 {
        self.matmul_nanos
    }

    /// Run one token through the decoder and keep its logits.
    pub fn forward(&mut self, token: u32) -> Result<()> {
        let dims = self.model.dims;
        if token as usize >= dims.vocab {
            return Err(Error::new(format!(
                "model: token {token} is outside the vocabulary"
            )));
        }
        let positions = self
            .position
            .checked_add(1)
            .ok_or_else(|| Error::new("model: context position overflow"))?;
        if positions > self.capacity {
            return Err(Error::new("model: context exceeds the key value cache"));
        }
        let kv_dim = dims.kv_heads * dims.head_dim;
        let cache_stride = self.capacity * kv_dim;

        self.model.embed.read_row(token as usize, &mut self.x)?;
        let mut clock = MatmulClock::new();

        for (index, layer) in self.model.layers.iter().enumerate() {
            layer.input_norm.read_vector(&mut self.norm_weight)?;
            rms_norm(&self.x, &self.norm_weight, dims.eps, &mut self.norm)?;
            clock.run(|| project(&layer.q_proj, &self.norm, &mut self.q))?;
            clock.run(|| project(&layer.k_proj, &self.norm, &mut self.k))?;
            clock.run(|| project(&layer.v_proj, &self.norm, &mut self.v))?;

            for head in 0..dims.q_heads {
                let start = head * dims.head_dim;
                rope(
                    &mut self.q[start..start + dims.head_dim],
                    self.position,
                    dims.rope_theta,
                    dims.rope_interleaved,
                )?;
            }
            for head in 0..dims.kv_heads {
                let start = head * dims.head_dim;
                rope(
                    &mut self.k[start..start + dims.head_dim],
                    self.position,
                    dims.rope_theta,
                    dims.rope_interleaved,
                )?;
            }

            let layer_base = index * cache_stride;
            let position_base = layer_base + self.position * kv_dim;
            self.keys[position_base..position_base + kv_dim].copy_from_slice(&self.k);
            self.values[position_base..position_base + kv_dim].copy_from_slice(&self.v);

            self.attn.fill(0.0);
            let scale = 1.0 / (dims.head_dim as f32).sqrt();
            let group = dims.group_size();
            for head in 0..dims.q_heads {
                let kv_head = head / group;
                let q_start = head * dims.head_dim;
                let kv_start = kv_head * dims.head_dim;
                for (position, score) in self.scores[..positions].iter_mut().enumerate() {
                    let key =
                        &self.keys[layer_base + position * kv_dim + kv_start..][..dims.head_dim];
                    let mut sum = 0.0f32;
                    for element in 0..dims.head_dim {
                        sum += self.q[q_start + element] * key[element];
                    }
                    *score = sum * scale;
                }
                softmax(&mut self.scores[..positions])?;
                for position in 0..positions {
                    let value =
                        &self.values[layer_base + position * kv_dim + kv_start..][..dims.head_dim];
                    let weight = self.scores[position];
                    for element in 0..dims.head_dim {
                        self.attn[q_start + element] += weight * value[element];
                    }
                }
            }

            clock.run(|| project(&layer.o_proj, &self.attn, &mut self.projected))?;
            for (slot, value) in self.x.iter_mut().zip(&self.projected) {
                *slot += value;
            }

            layer.post_norm.read_vector(&mut self.norm_weight)?;
            rms_norm(&self.x, &self.norm_weight, dims.eps, &mut self.norm)?;
            clock.run(|| project(&layer.gate_proj, &self.norm, &mut self.gate))?;
            clock.run(|| project(&layer.up_proj, &self.norm, &mut self.up))?;
            for (gate, up) in self.gate.iter_mut().zip(&self.up) {
                *gate = silu(*gate) * up;
            }
            clock.run(|| project(&layer.down_proj, &self.gate, &mut self.projected))?;
            for (slot, value) in self.x.iter_mut().zip(&self.projected) {
                *slot += value;
            }
        }

        self.model.final_norm.read_vector(&mut self.norm_weight)?;
        rms_norm(&self.x, &self.norm_weight, dims.eps, &mut self.norm)?;
        clock.run(|| project(&self.model.output, &self.norm, &mut self.logits))?;
        self.matmul_macs = self.matmul_macs.saturating_add(clock.macs);
        self.matmul_nanos = self.matmul_nanos.saturating_add(clock.nanos);
        self.position = positions;
        Ok(())
    }

    /// Greedy sample: the largest logit wins, the first index wins a tie.
    pub fn sample(&self) -> Result<u32> {
        let index = argmax(&self.logits).ok_or_else(|| Error::new("model: no logits to sample"))?;
        u32::try_from(index).map_err(|_| Error::new("model: sampled token does not fit u32"))
    }

    /// The logits of the last forward pass, indexed by token id.
    ///
    /// The masked sampler in the runtime reads them to pick the largest value
    /// among the tokens a grammar allows; the plain sampler keeps using
    /// [`Self::sample`].
    pub fn logits(&self) -> &[f32] {
        &self.logits
    }
}

/// Multiply one matrix by a vector and return the multiply accumulates.
///
/// A `bfloat16` matrix goes to the blocked kernel in one contiguous range per
/// call. Only the rows that reach into the final partial page use the element
/// accessor, because those bytes are not part of the mapping.
fn project(matrix: &Tensor<'_>, input: &[f32], out: &mut [f32]) -> Result<u64> {
    let rows = matrix.shape().first().copied().unwrap_or(0);
    let columns = matrix.shape().last().copied().unwrap_or(0);
    if rows == 0 || columns == 0 || input.len() != columns || out.len() < rows {
        return Err(Error::new(format!(
            "model: projection of {} has a shape that disagrees with its buffers",
            matrix.name
        )));
    }
    let mut mapped_rows = 0;
    if let Some((weights, rows_in_mapping)) = matrix.contiguous_bf16() {
        let length = rows_in_mapping
            .checked_mul(columns)
            .and_then(|elements| elements.checked_mul(2))
            .ok_or_else(|| Error::new("model: contiguous weight range overflow"))?;
        let bytes = weights
            .get(..length)
            .ok_or_else(|| Error::new("model: contiguous weight range is short"))?;
        matvec::matvec_bf16(
            bytes,
            rows_in_mapping,
            columns,
            input,
            &mut out[..rows_in_mapping],
        )?;
        mapped_rows = rows_in_mapping;
    }
    for (row, slot) in out.iter_mut().enumerate().take(rows).skip(mapped_rows) {
        *slot = matrix.dot(row, input);
    }
    Ok(matvec::macs(rows, columns))
}

/// Count the decoder layers by the highest `model.layers.N` in the directory.
fn count_layers(file: &ModelFile) -> Result<usize> {
    let mut layers = 0usize;
    for tensor in file.header.tensors() {
        let name = file.header.name(tensor);
        let Some(rest) = name.strip_prefix("model.layers.") else {
            continue;
        };
        let Some((index, _)) = rest.split_once('.') else {
            continue;
        };
        let index = index
            .parse::<usize>()
            .map_err(|_| Error::new(format!("model: bad layer index in {name}")))?;
        layers = layers.max(index + 1);
    }
    Ok(layers)
}

fn layer_tensor<'a>(file: &'a ModelFile, index: usize, suffix: &str) -> Result<Tensor<'a>> {
    let name = format!("model.layers.{index}.{suffix}");
    file.tensor(&name)
}

fn require_matrix(tensor: &Tensor<'_>, rows: usize, cols: usize, name: &str) -> Result<()> {
    if tensor.shape() != [rows, cols] {
        return Err(Error::new(format!(
            "model: {name} has shape {:?}, expected [{rows}, {cols}]",
            tensor.shape()
        )));
    }
    Ok(())
}

fn require_vector(tensor: &Tensor<'_>, length: usize, name: &str) -> Result<()> {
    if tensor.shape() != [length] {
        return Err(Error::new(format!(
            "model: {name} has shape {:?}, expected [{length}]",
            tensor.shape()
        )));
    }
    Ok(())
}
