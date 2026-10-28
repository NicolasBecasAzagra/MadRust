# The quantized checkpoint

`madrust-infer` reads two checkpoints: the `bfloat16` one a training stack
writes, and a quantized one that the runtime itself produces. This document is
the format of the second. Another implementation can read the same file from
it.

The checkpoint is a `safetensors` file. The tensor directory, the eight byte
little endian header length and the raw data after the header are unchanged;
only the encoding of a two dimensional tensor differs.

The format has two widths that share one block layout:

| Name | Weight | Block | Bytes per 32 weights | 7B parameters |
| --- | --- | --- | --- | --- |
| `Q4B32` | 4 bits | 32 weights, one binary16 scale | 18 (4.5 bits/weight) | 3.9 GB |
| `Q8B32` | 8 bits | 32 weights, one binary16 scale | 34 (8.5 bits/weight) | 7.4 GB |

`Q4B32` is the size format, and the default of the tool: it is the one that lets
a model of seven billion parameters fit in four gigabytes. `Q8B32` is the
fidelity format: it is the width at which this checkpoint keeps the answer of
the floating point one (see [Measured on this host](#measured-on-this-host)).
The two exist together because the measurement, not the assumption, decides
which one a checkpoint needs.

## The block

A matrix of `rows` by `columns` weights is a sequence of blocks of **32
consecutive weights of one row**. A block always starts with the scale and
continues with the payload:

| Offset | `Q4B32` | `Q8B32` |
| --- | --- | --- |
| 0 | 2 bytes: scale, IEEE 754 binary16, little endian | same |
| 2 | 16 bytes: 32 nibbles | 32 bytes: 32 signed bytes |

The block is 18 bytes for four bits and 34 bytes for eight bits. `columns` must
be a multiple of 32: the format has no partial block, so a matrix with a ragged
row is refused instead of padded. Blocks are stored row by row, left to right.

### The payload

The four bit payload is a two's complement nibble in `-8..=7`. Nibble `n` of a
block is the weight at position `block * 32 + n`; the **low** nibble of a byte
is the even position and the **high** nibble is the odd one:

```text
nibble = (byte >> (4 * (n % 2))) & 0x0f
q      = (nibble ^ 8) - 8          # sign extension
weight = q * half_to_f32(scale)
```

The eight bit payload is one two's complement byte per weight, in the same
order. The decoded weight is `q * scale` in both widths.

### The scale

The scale of a block is **the largest absolute value of the block divided by the
largest positive level**, rounded to binary16:

```text
levels = 7    for Q4B32     # the range is -8..=7
levels = 127  for Q8B32     # the range is -128..=127
scale  = f32_to_f16(max(|w| for w in block) / levels)
q      = round(w / f32(f16_to_f32(scale)))
```

The quantized value is clamped to the range. A block of zeros stores a scale of
`0.0` and a payload of zero.

Binary16 is enough for the scale: it has ten mantissa bits, so the scale is
rounded to a relative error below `2^-11`, while the weight is rounded to
`1/14` (four bits) or `1/254` (eight bits) of the block range. The extra two
bytes of a binary32 scale would bring `Q4B32` to five bits per weight and a
seven billion parameter model to 4.4 GB instead of 3.9 GB, which is the size
the format exists to avoid.

## The file

The `safetensors` header of a quantized checkpoint carries the declaration of
the width in `__metadata__`. The values are strings, as the format requires:

```json
"__metadata__": {
  "format": "madrust-q4b32",
  "block": "32",
  "scale": "f16"
}
```

`format` is `madrust-q4b32` when every quantized tensor is `Q4B32`, and
`madrust-q8b32` when every quantized tensor is `Q8B32`. One file carries one
width. Every two dimensional tensor has the `dtype` of that width, and its
`data_offsets` cover exactly

```text
Q4B32: rows * (columns / 32) * 18 bytes
Q8B32: rows * (columns / 32) * 34 bytes
```

The data is the blocks of the shape, row by row. One dimensional tensors, the
RMSNorm weights, keep their floating point type (`BF16` or `F32`): a norm weight
is one value per hidden channel, and quantizing it would move the scale of the
whole residual stream.

A reader of this runtime refuses a quantized checkpoint when

- a file mixes `Q4B32` and `Q8B32` tensors;
- `__metadata__` is absent, or `format`, `block` or `scale` is not the value
  the width requires;
- the tensor is not two dimensional, or its columns are not a multiple of 32;
- the declared byte length disagrees with the shape and the width;
- a block scale is not a finite binary16.

The error names the tensor, the field and, for a scale, the row and the block.

## Why the matrix vector product decodes in place

A weight is read once per token, so the block is decoded inside the product
instead of into a temporary floating point matrix:

```text
for each row:
    for each block of the row:
        scale = half_to_f32(block[0..2])
        accumulator = sum over the 32 weights of q * input
        row_sum += accumulator * scale
```

The block is the only weight traffic; the alternative, expanding the row to
`f32` first, would write and read 128 bytes for the same 32 weights and need a
buffer per tensor. The k-th weight of a block is read at `2 + k` (eight bit) or
`2 + k / 2` (four bit), so a processor without a wide instruction still walks
the matrix once. `src/matvec.rs` has a test per width that compares the product
of a known matrix with a known vector against the value a reference
implementation gives for the same dequantized matrix, and the benchmark binary
`madrust-infer-matmul` measures the same loops without a model.

## Producing and checking a checkpoint

```sh
# Build the tool on the host and quantize the checkpoint in place.
cargo run --release --manifest-path madrust-infer/Cargo.toml \
    --bin madrust-infer-quantize -- \
    --bits 4 \
    --in "$HOME/madrust/artifacts/model" \
    --out "$HOME/madrust/artifacts/model-q4"

# The runtime reads the output directory as it is: the tool copies
# config.json, vocab.json and merges.txt next to the weights. `--bits 8`
# writes the fidelity width instead; four bits is the default.
```

The tool prints the number of tensors it rewrote, the ratio against the source
and the SHA-256 digest of the file it wrote:

```text
QUANT model=.../model-q4/model.safetensors bytes=75745552 source_bytes=269060544 \
      tensors=272 quantized=211 kept=61 bits=4 block=32 scale=f16
QUANT ratio=0.282 digest=96e71116453f4b7f7b6a930299e8192e28682dda7dfc2af248c7952afdeab874
```

`madrust-infer-compare` decodes the published prompt with both checkpoints and
prints the tokens of each and the largest and mean absolute difference of the
logits. The guest passes no arguments, so it opens `/model.safetensors` and
`/model-q4.safetensors` under the initramfs root; a host run can pass the three
paths:

```text
COMPARE tokens_float 260 3575 282 260 1798 30 198 198
COMPARE tokens_quant 260 3575 282 260 1798 30 198 198
COMPARE reference_float=true reference_quant=true
COMPARE logits steps=8 values=393216 max=0.543618 mean=0.093776788
COMPARE prefill max=0.458115 mean=0.072351724
COMPARE result match=true tokens=8 max_diff=0.543618 mean_diff=0.093776788
COMPARE OK
```

The token lists are the two independent greedy decodes. The `logits` line is
the difference on the floating point token sequence, so every step compares the
same input, and `prefill` is the difference after the prompt alone. The tool
exits non-zero when the token lists differ.

The continuous integration does not need the download: `make test` runs the
gate in `src/gate.rs` on Linux. It writes a checkpoint with the shape of the
development model of `madrust-infer-gen`, quantizes it with the same code the
tool runs, decodes the published prompt with both checkpoints and requires the
same tokens and a logits difference below `1e-3`.

The gate weights are exact in the block format: every block of thirty-two
carries a `+7` or a `-7`, so the scale is one and every payload value is an
integer. The four bit checkpoint is therefore the floating point tensor, and
the gate compares the decoder against the floating point path instead of
comparing two different models. A random checkpoint cannot carry a token gate:
its greedy answer turns on margins as small as `0.003`, which no four bit
encoding can promise to keep.

## Measured on this host

`The capital of France is` (prompt ids `504 3575 282 4649 314`), greedy, eight
tokens, `SmolLM2-135M` at `93efa2f097d58c2a74874c7e644dbc9b0cee75a2`:

| Checkpoint | File | Mapped | Tokens |
| --- | --- | --- | --- |
| `bfloat16` | 269,060,552 B | 269,058,048 B | `260 3575 282 260 1798 30 198 198` |
| `Q4B32` | 75,745,552 B | 75,743,232 B | `260 198 29721 282 4649 30 198 198` |
| `Q8B32` | 142,985,696 B | 142,983,168 B | `260 3575 282 260 1798 30 198 198` |

The file falls to `0.282` of the source at four bits and `0.531` at eight bits,
and the mapped bytes fall by the same factor. The floating point tokens are the
published reference run of `bench/reference/`; the four bit checkpoint is **not**
the same answer, and the eight bit one is.

The difference of the logits, measured with both checkpoints on the floating
point token sequence so that every step compares the same input:

| Checkpoint | Prefill max | Prefill mean | Steps max | Steps mean |
| --- | --- | --- | --- | --- |
| `Q4B32` | 7.102161 | 1.562760003 | 11.669730 | 2.083776259 |
| `Q8B32` | 0.458115 | 0.072351724 | 0.543618 | 0.093776788 |

A block of thirty-two weights with one four bit scale has a relative error of
about `0.11` of the weight standard deviation, and the greedy margins of this
prompt reach `0.052` at the seventh token. No four bit encoding closes that gap:
a numpy reference run with the same block layout and the same maths reproduces
the tokens at eight bits per weight and diverges at six, five and four bits.
The four bit checkpoint is the size the milestone needs; the eight bit one is
the checkpoint that keeps the answer. Both are produced by the same tool, read
by the same decoder and measured by the same rig, so the choice is a flag and
not a second implementation.

The resident memory of the guest and the token rate of both widths are in the
rows the rig publishes: `bench/results/quantization/` holds the floating point
and `Q8B32` runs of both kernels, and `bench/results/quantization-q4/` holds
the floating point and `Q4B32` runs of MadRust. The block kernels are scalar,
so on a host where `bfloat16` runs on AVX2 the checkpoint is slower per token
even though it streams fewer bytes: the format buys memory, and the rig rows
say so next to the rate. The guest resident memory of the quantized boot is
below the floating point one by the size of the archive it no longer carries,
and the `INFER pinned bytes=` line of the runtime is the exact weight
footprint of the format.
