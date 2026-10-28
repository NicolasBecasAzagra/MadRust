// SPDX-License-Identifier: GPL-2.0-only
//! The `Q4B32` block format: four bit weights with one scale per block.
//!
//! A matrix is stored as a sequence of blocks of [`BLOCK`] consecutive weights
//! along a row. A block is a two byte IEEE binary16 scale followed by
//! [`PACKED_BYTES`] bytes of nibbles, so it costs 4.5 bits per weight. The
//! quantized weight is `q * scale` with `q` a signed four bit integer in
//! `-8..=7`, and the scale is the largest absolute value of the block divided
//! by seven, so the largest weight of a block lands on `+7` and the smallest
//! step of the block is `max_abs / 7`.
//!
//! The scale is binary16 and not binary32 because the two extra bytes per block
//! would bring the format to five bits per weight, which is the size the
//! milestone rules out for seven billion parameters. Binary16 keeps ten
//! mantissa bits, which is far more precision than the four bits of the weight:
//! rounding the scale moves a block by at most `2^-11` of its range, while
//! rounding the weight moves it by up to `1/14` of its range.
//!
//! The layout is documented for another implementation in
//! `docs/QUANTIZATION.md`. This module owns everything the format means: the
//! half precision conversion, the block packing, the block unpacking, and the
//! exact number of bytes a matrix occupies. It has no I/O and no architecture
//! specific code, so it is tested on the host.

use crate::error::{Error, Result};

/// Weights in one block.
pub const BLOCK: usize = 32;

/// Bytes of the binary16 scale of a block.
pub const SCALE_BYTES: usize = 2;

/// Bytes of the sixteen packed nibbles of a block.
pub const PACKED_BYTES: usize = BLOCK / 2;

/// Bytes of one complete block.
pub const BLOCK_BYTES: usize = SCALE_BYTES + PACKED_BYTES;

/// Bytes of the eight bit payload of a block.
///
/// [`Q8B32`](Q8_BLOCK_BYTES) is the same block layout at one byte per weight.
/// It exists because the four bit block does not keep the greedy answer of
/// every checkpoint: the eight bit form is the one the runtime produces when
/// the answer has to stay identical, and it still halves the weights against
/// `bfloat16`.
pub const Q8_PACKED_BYTES: usize = BLOCK;

/// Bytes of one eight bit block: the binary16 scale and the signed bytes.
pub const Q8_BLOCK_BYTES: usize = SCALE_BYTES + Q8_PACKED_BYTES;

/// Name of the eight bit format as the checkpoint metadata spells it.
pub const Q8_FORMAT_NAME: &str = "madrust-q8b32";

/// Largest eight bit quantized value.
const Q8_MAX_QUANT: f32 = 127.0;

/// Larger magnitudes are clamped to the positive end of the four bit range.
const MAX_QUANT: f32 = 7.0;

/// Smallest quantized value, kept for the documentation of the range.
const MIN_QUANT: i32 = -8;

/// Largest quantized value.
const MAX_QUANT_INT: i32 = 7;

/// Name of the format as the checkpoint metadata spells it.
pub const FORMAT_NAME: &str = "madrust-q4b32";

/// Name of the scale type as the checkpoint metadata spells it.
pub const SCALE_NAME: &str = "f16";

/// Number of blocks one row of `columns` weights occupies.
pub fn blocks(columns: usize) -> usize {
    columns.div_ceil(BLOCK)
}

/// Bytes one row of `columns` weights occupies.
///
/// `columns` must be a multiple of [`BLOCK`]: the format has no partial block,
/// so a matrix with a ragged row is refused instead of padded.
pub fn row_bytes(columns: usize) -> Result<usize> {
    if columns == 0 || !columns.is_multiple_of(BLOCK) {
        return Err(Error::new(format!(
            "quant: {columns} columns is not a multiple of the {BLOCK} weight block"
        )));
    }
    Ok(blocks(columns) * BLOCK_BYTES)
}

/// Bytes a `rows` by `columns` matrix occupies.
pub fn matrix_bytes(rows: usize, columns: usize) -> Result<usize> {
    let row = row_bytes(columns)?;
    rows.checked_mul(row)
        .ok_or_else(|| Error::new("quant: matrix byte length overflow"))
}

/// Bytes one row of `columns` weights occupies in the eight bit form.
pub fn q8_row_bytes(columns: usize) -> Result<usize> {
    if columns == 0 || !columns.is_multiple_of(BLOCK) {
        return Err(Error::new(format!(
            "quant: {columns} columns is not a multiple of the {BLOCK} weight block"
        )));
    }
    Ok(blocks(columns) * Q8_BLOCK_BYTES)
}

/// Bytes a `rows` by `columns` eight bit matrix occupies.
pub fn q8_matrix_bytes(rows: usize, columns: usize) -> Result<usize> {
    let row = q8_row_bytes(columns)?;
    rows.checked_mul(row)
        .ok_or_else(|| Error::new("quant: matrix byte length overflow"))
}

/// Convert an IEEE binary16 bit pattern to the nearest `f32`.
///
/// Every binary16 value is exactly representable in `f32`, so the conversion is
/// exact for the whole range, subnormals included.
pub fn half_to_f32(half: u16) -> f32 {
    let sign = ((half as u32) & 0x8000) << 16;
    let exponent = ((half >> 10) & 0x1f) as u32;
    let mantissa = (half & 0x03ff) as u32;
    let bits = if exponent == 0 {
        if mantissa == 0 {
            sign
        } else {
            // Subnormal: normalize the mantissa and lower the exponent.
            let mut value = mantissa;
            let mut shift = 0u32;
            while value & 0x0400 == 0 {
                value <<= 1;
                shift += 1;
            }
            sign | ((113 - shift) << 23) | ((value & 0x03ff) << 13)
        }
    } else if exponent == 0x1f {
        // Infinity or a quiet NaN, both with a non-zero mantissa preserved.
        sign | 0x7f80_0000 | (mantissa << 13)
    } else {
        sign | ((exponent + 127 - 15) << 23) | (mantissa << 13)
    };
    f32::from_bits(bits)
}

/// Convert `value` to the nearest binary16 bit pattern, ties to even.
///
/// The quantizer uses the rounded value the decoder will read, so a scale that
/// binary16 cannot hold exactly does not bias the nibbles it produces.
pub fn f32_to_half(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        return sign
            | 0x7c00
            | if mantissa == 0 {
                0
            } else {
                0x0200 | ((mantissa >> 13) as u16)
            };
    }
    let unbiased = exponent - 127;
    if unbiased > 15 {
        return sign | 0x7c00;
    }
    if unbiased >= -14 {
        let half_exponent = ((unbiased + 15) as u32) << 10;
        let half_mantissa = mantissa >> 13;
        let remainder = mantissa & 0x1fff;
        let mut result = half_exponent | half_mantissa;
        if remainder > 0x1000 || (remainder == 0x1000 && half_mantissa & 1 == 1) {
            result += 1;
        }
        return sign | result as u16;
    }
    if unbiased < -25 {
        return sign;
    }
    // Subnormal: the target mantissa is `m * 2^24`, where `m` is the value.
    let full = mantissa | 0x0080_0000;
    let shift = (-1 - unbiased) as u32;
    let truncated = full >> shift;
    let remainder = full & ((1u32 << shift) - 1);
    let halfway = 1u32 << (shift - 1);
    let mut result = truncated;
    if remainder > halfway || (remainder == halfway && truncated & 1 == 1) {
        result += 1;
    }
    sign | result as u16
}

/// Quantize one block of `BLOCK` values into one block of the file.
///
/// The scale is stored first, as binary16, then the nibbles: the low nibble of
/// byte `2 + i` is weight `2i` and the high nibble is weight `2i + 1`. A nibble
/// is the two's complement four bit value of the quantized weight.
pub fn quantize_block(values: &[f32], out: &mut [u8]) -> Result<()> {
    if values.len() != BLOCK || out.len() != BLOCK_BYTES {
        return Err(Error::new(format!(
            "quant: a block is {BLOCK} values into {BLOCK_BYTES} bytes"
        )));
    }
    let mut largest = 0.0f32;
    for &value in values {
        if !value.is_finite() {
            return Err(Error::new("quant: a weight is not finite"));
        }
        largest = largest.max(value.abs());
    }
    // The scale used for the nibbles is the one the decoder reads back, so the
    // largest weight uses the whole positive range instead of an unrounded
    // approximation of it.
    let scale = if largest == 0.0 {
        0.0
    } else {
        half_to_f32(f32_to_half(largest / MAX_QUANT))
    };
    out[..SCALE_BYTES].copy_from_slice(&f32_to_half(largest / MAX_QUANT).to_le_bytes());
    let inverse = if scale == 0.0 { 0.0 } else { 1.0 / scale };
    out[SCALE_BYTES..].fill(0);
    for (index, &value) in values.iter().enumerate() {
        let quantized = if scale == 0.0 {
            0
        } else {
            (value * inverse)
                .round()
                .clamp(MIN_QUANT as f32, MAX_QUANT_INT as f32) as i32
        };
        let nibble = (quantized & 0x0f) as u8;
        let shift = 4 * (index % 2);
        out[SCALE_BYTES + index / 2] |= nibble << shift;
    }
    Ok(())
}

/// Decode one block into `BLOCK` values.
pub fn dequantize_block(block: &[u8], out: &mut [f32]) -> Result<()> {
    if block.len() != BLOCK_BYTES || out.len() != BLOCK {
        return Err(Error::new(format!(
            "quant: a block is {BLOCK_BYTES} bytes into {BLOCK} values"
        )));
    }
    let scale = half_to_f32(u16::from_le_bytes([block[0], block[1]]));
    if !scale.is_finite() {
        return Err(Error::new("quant: a block scale is not finite"));
    }
    for (index, slot) in out.iter_mut().enumerate() {
        let packed = block[SCALE_BYTES + index / 2];
        let nibble = (packed >> (4 * (index % 2))) & 0x0f;
        let value = ((nibble as i8) << 4 >> 4) as f32;
        *slot = value * scale;
    }
    Ok(())
}

/// Decode a whole row of blocks into `out`.
pub fn dequantize_row(blocks: &[u8], out: &mut [f32]) -> Result<()> {
    let columns = out.len();
    let expected = row_bytes(columns)?;
    if blocks.len() != expected {
        return Err(Error::new(format!(
            "quant: a row of {columns} weights is {expected} bytes, not {}",
            blocks.len()
        )));
    }
    for (index, chunk) in out.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
        let start = index * BLOCK_BYTES;
        dequantize_block(&blocks[start..start + BLOCK_BYTES], chunk)?;
    }
    Ok(())
}

/// `true` when a binary16 bit pattern is finite.
pub fn scale_is_finite(half: u16) -> bool {
    half & 0x7c00 != 0x7c00
}

/// Quantize one block of `BLOCK` values into one eight bit block.
///
/// The scale is the largest magnitude of the block divided by 127, rounded to
/// binary16, and each weight is one signed byte of the block. The layout is the
/// four bit one with a wider payload, so a reader that knows the block size
/// reads both with the same loop.
pub fn quantize_block_q8(values: &[f32], out: &mut [u8]) -> Result<()> {
    if values.len() != BLOCK || out.len() != Q8_BLOCK_BYTES {
        return Err(Error::new(format!(
            "quant: an eight bit block is {BLOCK} values into {Q8_BLOCK_BYTES} bytes"
        )));
    }
    let mut largest = 0.0f32;
    for &value in values {
        if !value.is_finite() {
            return Err(Error::new("quant: a weight is not finite"));
        }
        largest = largest.max(value.abs());
    }
    let scale = if largest == 0.0 {
        0.0
    } else {
        half_to_f32(f32_to_half(largest / Q8_MAX_QUANT))
    };
    out[..SCALE_BYTES].copy_from_slice(&f32_to_half(largest / Q8_MAX_QUANT).to_le_bytes());
    let inverse = if scale == 0.0 { 0.0 } else { 1.0 / scale };
    for (index, &value) in values.iter().enumerate() {
        let quantized = if scale == 0.0 {
            0
        } else {
            (value * inverse).round().clamp(-128.0, 127.0) as i8
        };
        out[SCALE_BYTES + index] = quantized as u8;
    }
    Ok(())
}

/// Decode one eight bit block into `BLOCK` values.
pub fn dequantize_block_q8(block: &[u8], out: &mut [f32]) -> Result<()> {
    if block.len() != Q8_BLOCK_BYTES || out.len() != BLOCK {
        return Err(Error::new(format!(
            "quant: an eight bit block is {Q8_BLOCK_BYTES} bytes into {BLOCK} values"
        )));
    }
    let scale = half_to_f32(u16::from_le_bytes([block[0], block[1]]));
    if !scale.is_finite() {
        return Err(Error::new("quant: a block scale is not finite"));
    }
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = (block[SCALE_BYTES + index] as i8) as f32 * scale;
    }
    Ok(())
}

/// Decode a whole eight bit row of blocks into `out`.
pub fn dequantize_row_q8(blocks: &[u8], out: &mut [f32]) -> Result<()> {
    let columns = out.len();
    let expected = q8_row_bytes(columns)?;
    if blocks.len() != expected {
        return Err(Error::new(format!(
            "quant: an eight bit row of {columns} weights is {expected} bytes, not {}",
            blocks.len()
        )));
    }
    for (index, chunk) in out.as_chunks_mut::<BLOCK>().0.iter_mut().enumerate() {
        let start = index * Q8_BLOCK_BYTES;
        dequantize_block_q8(&blocks[start..start + Q8_BLOCK_BYTES], chunk)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        blocks, dequantize_block, dequantize_block_q8, dequantize_row, dequantize_row_q8,
        f32_to_half, half_to_f32, matrix_bytes, q8_matrix_bytes, q8_row_bytes, quantize_block,
        quantize_block_q8, row_bytes, scale_is_finite, BLOCK, BLOCK_BYTES, Q8_BLOCK_BYTES,
    };

    #[test]
    fn half_conversion_covers_the_known_values() {
        // Exact values, the largest finite binary16, the smallest normal, a
        // subnormal and the two infinities.
        for (half, value) in [
            (0x0000u16, 0.0f32),
            (0x3c00, 1.0),
            (0xbc00, -1.0),
            (0x3800, 0.5),
            (0x7bff, 65_504.0),
            (0x0400, 6.103_515_6e-5),
            (0x0001, 5.960_464_5e-8),
            (0x7c00, f32::INFINITY),
            (0xfc00, f32::NEG_INFINITY),
        ] {
            assert_eq!(half_to_f32(half), value, "half {half:#06x}");
        }
        assert_eq!(f32_to_half(1.0), 0x3c00);
        assert_eq!(f32_to_half(-2.0), 0xc000);
        assert_eq!(f32_to_half(65_504.0), 0x7bff);
        assert_eq!(f32_to_half(0.5), 0x3800);
        assert_eq!(f32_to_half(6.0e-8), 0x0001);
        assert_eq!(f32_to_half(1.0e-9), 0x0000);
        assert_eq!(f32_to_half(f32::INFINITY), 0x7c00);
        // A value above the range saturates to infinity instead of wrapping.
        assert!(half_to_f32(f32_to_half(1.0e6)).is_infinite());
        assert_eq!(f32_to_half(f32::NEG_INFINITY), 0xfc00);
        // Every finite binary16 round trips through the conversion.
        for half in (0..0x7c00u16).chain(0x8000..0xfc00) {
            assert_eq!(f32_to_half(half_to_f32(half)), half, "half {half:#06x}");
        }
    }

    #[test]
    fn a_block_with_exact_values_round_trips() {
        // The largest magnitude is seven, so the scale is exactly one and every
        // nibble value is an integer. The round trip is exact.
        let mut values = [0.0f32; BLOCK];
        for (index, slot) in values.iter_mut().enumerate() {
            *slot = ((index as i32) % 15 - 7) as f32;
        }
        let mut block = [0u8; BLOCK_BYTES];
        quantize_block(&values, &mut block).unwrap();
        assert_eq!(u16::from_le_bytes([block[0], block[1]]), 0x3c00);
        let mut decoded = [0.0f32; BLOCK];
        dequantize_block(&block, &mut decoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn a_zero_block_stores_a_zero_scale() {
        let values = [0.0f32; BLOCK];
        let mut block = [0xffu8; BLOCK_BYTES];
        quantize_block(&values, &mut block).unwrap();
        assert_eq!(block, [0u8; BLOCK_BYTES]);
        let mut decoded = [1.0f32; BLOCK];
        dequantize_block(&block, &mut decoded).unwrap();
        assert_eq!(decoded, [0.0f32; BLOCK]);
    }

    #[test]
    fn the_quantization_error_stays_inside_half_a_step() {
        let values: Vec<f32> = (0..BLOCK)
            .map(|index| ((index as f32) * 0.37).sin() * 1.5)
            .collect();
        let mut block = [0u8; BLOCK_BYTES];
        quantize_block(&values, &mut block).unwrap();
        let scale = half_to_f32(u16::from_le_bytes([block[0], block[1]]));
        let mut decoded = [0.0f32; BLOCK];
        dequantize_block(&block, &mut decoded).unwrap();
        for (original, restored) in values.iter().zip(decoded) {
            assert!(
                (original - restored).abs() <= scale / 2.0 + 1e-6,
                "{} against {} with scale {scale}",
                original,
                restored
            );
        }
    }

    #[test]
    fn a_row_of_blocks_round_trips() {
        let columns = 3 * BLOCK;
        let values: Vec<f32> = (0..columns)
            .map(|index| ((index as f32) * 0.11).cos())
            .collect();
        let mut bytes = vec![0u8; row_bytes(columns).unwrap()];
        for (index, chunk) in values.as_chunks::<BLOCK>().0.iter().enumerate() {
            quantize_block(
                chunk,
                &mut bytes[index * BLOCK_BYTES..(index + 1) * BLOCK_BYTES],
            )
            .unwrap();
        }
        let mut decoded = vec![0.0f32; columns];
        dequantize_row(&bytes, &mut decoded).unwrap();
        for (index, (original, restored)) in values.iter().zip(&decoded).enumerate() {
            // One scale per block, so the bound is half a step of that block.
            let block = &values[index / BLOCK * BLOCK..(index / BLOCK + 1) * BLOCK];
            let mut largest = 0.0f32;
            for &value in block {
                largest = largest.max(value.abs());
            }
            assert!((original - restored).abs() <= largest / 14.0 + 1e-6);
        }
    }

    #[test]
    fn row_and_matrix_sizes_are_the_block_layout() {
        assert_eq!(blocks(BLOCK), 1);
        assert_eq!(blocks(576), 18);
        assert_eq!(row_bytes(576).unwrap(), 18 * 18);
        assert_eq!(matrix_bytes(3, 32).unwrap(), 3 * 18);
        assert!(row_bytes(31).is_err());
        assert!(row_bytes(0).is_err());
        assert!(matrix_bytes(usize::MAX, 32).is_err());
    }

    #[test]
    fn an_eight_bit_block_round_trips_and_bounds_its_error() {
        let mut values = [0.0f32; BLOCK];
        for (index, slot) in values.iter_mut().enumerate() {
            *slot = ((index as f32) * 0.043).sin() * 2.0;
        }
        let mut block = [0u8; Q8_BLOCK_BYTES];
        quantize_block_q8(&values, &mut block).unwrap();
        let scale = half_to_f32(u16::from_le_bytes([block[0], block[1]]));
        let mut decoded = [0.0f32; BLOCK];
        dequantize_block_q8(&block, &mut decoded).unwrap();
        for (original, restored) in values.iter().zip(decoded) {
            assert!((original - restored).abs() <= scale / 2.0 + 1e-6);
        }
        // A largest magnitude of 127 gives a scale of exactly one, so the
        // integer payload can be read back value by value.
        let exact = [127.0f32; BLOCK];
        quantize_block_q8(&exact, &mut block).unwrap();
        dequantize_block_q8(&block, &mut decoded).unwrap();
        assert_eq!(decoded, exact);
    }

    #[test]
    fn an_eight_bit_row_uses_the_wider_layout() {
        assert_eq!(q8_row_bytes(576).unwrap(), 18 * Q8_BLOCK_BYTES);
        assert_eq!(q8_matrix_bytes(3, 32).unwrap(), 3 * Q8_BLOCK_BYTES);
        let columns = 2 * BLOCK;
        let values: Vec<f32> = (0..columns)
            .map(|index| ((index as f32) * 0.19).cos())
            .collect();
        let mut bytes = vec![0u8; q8_row_bytes(columns).unwrap()];
        for (index, chunk) in values.as_chunks::<BLOCK>().0.iter().enumerate() {
            quantize_block_q8(
                chunk,
                &mut bytes[index * Q8_BLOCK_BYTES..(index + 1) * Q8_BLOCK_BYTES],
            )
            .unwrap();
        }
        let mut decoded = vec![0.0f32; columns];
        dequantize_row_q8(&bytes, &mut decoded).unwrap();
        for (original, restored) in values.iter().zip(&decoded) {
            assert!((original - restored).abs() < 0.02);
        }
    }

    #[test]
    fn rejects_a_block_of_the_wrong_length_and_a_ragged_row() {
        assert!(quantize_block(&[0.0; BLOCK - 1], &mut [0u8; BLOCK_BYTES]).is_err());
        assert!(quantize_block(&[0.0; BLOCK], &mut [0u8; BLOCK_BYTES - 1]).is_err());
        assert!(dequantize_block(&[0u8; BLOCK_BYTES], &mut [0.0; BLOCK - 1]).is_err());
        assert!(dequantize_row(&[0u8; BLOCK_BYTES], &mut [0.0; BLOCK + 1]).is_err());
    }

    #[test]
    fn rejects_a_weight_that_is_not_finite() {
        let mut values = [0.0f32; BLOCK];
        values[3] = f32::NAN;
        assert!(quantize_block(&values, &mut [0u8; BLOCK_BYTES]).is_err());
        // A non-finite scale is refused when the block is decoded.
        let mut block = [0u8; BLOCK_BYTES];
        block[..2].copy_from_slice(&0x7c00u16.to_le_bytes());
        assert!(!scale_is_finite(0x7c00));
        assert!(dequantize_block(&block, &mut [0.0f32; BLOCK]).is_err());
    }
}
