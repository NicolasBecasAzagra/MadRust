// SPDX-License-Identifier: GPL-2.0-only
//! Blocked vector by matrix multiplication over `bfloat16` weights.
//!
//! A projection is `out[r] = dot(weight[r], input)` with a row major matrix
//! `[rows, columns]`. The output is tiled by [`ROW_TILE`] rows and the columns
//! are walked in [`COLUMN_TILE`] chunks. A tile of four rows of the widest
//! matrix in the reference model (1536 columns) is 12 KiB of weights plus 2 KiB
//! of input, so the whole tile stays in the first level cache while it is
//! consumed. The input chunk is loaded once per tile chunk and used by four
//! rows, and each weight element is read once, in address order, on the one
//! pass the tile makes over the matrix. Nothing is read twice.
//!
//! The weights stay `bfloat16` in the mapping. The kernel widens them to `f32`
//! in registers as it reads them, which costs one shift and one widening per
//! weight and no extra memory traffic. A cached `f32` copy would double the
//! weight footprint from 269 MB to 538 MB for the reference model and double
//! the bytes streamed per token; the conversion instructions are cheaper than
//! that traffic, and the model is never copied out of the initramfs. The price
//! is that the arithmetic units do the conversion on every token instead of
//! once at load.
//!
//! Three vector paths exist. `x86_64` uses AVX2 and FMA when the processor and
//! the kernel report both, and SSE2 otherwise; SSE2 is part of the x86-64
//! baseline, so a kernel that does not enable the extended register state still
//! gets four wide arithmetic. `aarch64` uses the NEON baseline, so the same
//! kernel can be measured by a host benchmark on an ARM machine. The
//! architecture chooses the module at compile time and, on `x86_64`, the
//! feature detection chooses at run time. The scalar path is always available
//! and produces the same values to floating point tolerance. The vector paths
//! sum eight (or four) products per accumulator and the scalar path sums one,
//! so the rounding order differs; the tokens the runtime generates do not.

use crate::error::{Error, Result};

/// Rows of the output computed together.
pub const ROW_TILE: usize = 4;

/// Columns walked together. A multiple of the widest vector step, 8.
pub const COLUMN_TILE: usize = 512;

/// Multiply accumulates one matrix vector product performs.
pub fn macs(rows: usize, columns: usize) -> u64 {
    rows as u64 * columns as u64
}

/// Multiply `weights` by `input`, writing one value per row.
///
/// `weights` is a row major `bfloat16` matrix of `rows * columns` elements and
/// `input` has `columns` values. The caller validated the shapes; a length that
/// still disagrees is reported as an error rather than indexed.
#[cfg(target_arch = "x86_64")]
pub fn matvec_bf16(
    weights: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
    out: &mut [f32],
) -> Result<()> {
    validate(weights, rows, columns, input, out)?;
    if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma") {
        // SAFETY: the detection above reported both features, and `validate`
        // checked every length the kernel indexes.
        unsafe {
            x86::matvec(weights, rows, columns, input, out);
        }
    } else {
        // SSE2 is part of the x86-64 baseline, so no detection is needed. A
        // kernel that does not enable the extended register state reports
        // AVX2 as absent, and this path still uses four wide arithmetic.
        // SAFETY: `validate` checked every length the kernel indexes.
        unsafe {
            sse2::matvec(weights, rows, columns, input, out);
        }
    }
    Ok(())
}

#[cfg(target_arch = "aarch64")]
pub fn matvec_bf16(
    weights: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
    out: &mut [f32],
) -> Result<()> {
    validate(weights, rows, columns, input, out)?;
    // SAFETY: NEON is part of the aarch64 baseline, and `validate` checked
    // every length the kernel indexes.
    unsafe {
        neon::matvec(weights, rows, columns, input, out);
    }
    Ok(())
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn matvec_bf16(
    weights: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
    out: &mut [f32],
) -> Result<()> {
    validate(weights, rows, columns, input, out)?;
    matvec_scalar(weights, rows, columns, input, out);
    Ok(())
}

/// The scalar kernel, used by the benchmark as the baseline and by a processor
/// without a vector path.
pub fn matvec_scalar(weights: &[u8], rows: usize, columns: usize, input: &[f32], out: &mut [f32]) {
    for (row, slot) in out.iter_mut().enumerate().take(rows) {
        let mut sum = 0.0f32;
        for (column, &value) in input.iter().enumerate().take(columns) {
            let offset = (row * columns + column) * 2;
            let bits = u16::from_le_bytes([weights[offset], weights[offset + 1]]);
            sum += f32::from_bits((bits as u32) << 16) * value;
        }
        *slot = sum;
    }
}

/// Multiply a `Q4B32` matrix by `input`, writing one value per row.
///
/// The weights are never expanded into a floating point matrix: the kernel
/// reads one block of [`crate::quant::BLOCK_BYTES`] bytes, applies the scale of
/// that block to the sum of its thirty-two products, and moves on. The memory
/// traffic is 18 bytes for 32 weights, against 64 bytes for the same weights in
/// `bfloat16`, which is what the format buys.
///
/// The nibble is sign extended before the product and the block sum is scaled
/// once, so the rounding of the scale is not repeated thirty-two times.
pub fn matvec_q4(
    weights: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
    out: &mut [f32],
) -> Result<()> {
    validate_q4(weights, rows, columns, input, out)?;
    let row_bytes = crate::quant::row_bytes(columns)?;
    let blocks = crate::quant::blocks(columns);
    for (row, slot) in out.iter_mut().enumerate().take(rows) {
        let base = row * row_bytes;
        let mut sum = 0.0f32;
        for block in 0..blocks {
            let offset = base + block * crate::quant::BLOCK_BYTES;
            let scale = crate::quant::half_to_f32(u16::from_le_bytes([
                weights[offset],
                weights[offset + 1],
            ]));
            let mut accumulator = 0.0f32;
            let mut column = block * crate::quant::BLOCK;
            for pair in 0..crate::quant::PACKED_BYTES {
                let packed = weights[offset + crate::quant::SCALE_BYTES + pair];
                let low = ((packed & 0x0f) as i32) << 28 >> 28;
                let high = ((packed >> 4) as i32) << 28 >> 28;
                accumulator += low as f32 * input[column] + high as f32 * input[column + 1];
                column += 2;
            }
            sum += accumulator * scale;
        }
        *slot = sum;
    }
    Ok(())
}

/// Name of the vector kernel this build selects, for the serial log.
#[cfg(target_arch = "x86_64")]
pub fn kernel_name() -> &'static str {
    if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("fma") {
        "avx2+fma"
    } else {
        "sse2"
    }
}

/// Name of the vector kernel this build selects, for the serial log.
#[cfg(target_arch = "aarch64")]
pub fn kernel_name() -> &'static str {
    "neon"
}

/// Name of the vector kernel this build selects, for the serial log.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn kernel_name() -> &'static str {
    "scalar"
}

/// Multiply an eight bit block matrix by `input`, writing one value per row.
///
/// The loop is the four bit one with a wider payload: the block is read, its
/// scale is applied to the sum of the thirty-two products, and the weights are
/// never expanded into a floating point matrix. The reason the eight bit form
/// exists is fidelity, not speed: it costs 34 bytes per 32 weights instead of
/// 18, and it is the width that keeps the tokens of the reference checkpoint.
pub fn matvec_q8(
    weights: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
    out: &mut [f32],
) -> Result<()> {
    validate_q8(weights, rows, columns, input, out)?;
    let row_bytes = crate::quant::q8_row_bytes(columns)?;
    let blocks = crate::quant::blocks(columns);
    for (row, slot) in out.iter_mut().enumerate().take(rows) {
        let base = row * row_bytes;
        let mut sum = 0.0f32;
        for block in 0..blocks {
            let offset = base + block * crate::quant::Q8_BLOCK_BYTES;
            let scale = crate::quant::half_to_f32(u16::from_le_bytes([
                weights[offset],
                weights[offset + 1],
            ]));
            let mut accumulator = 0.0f32;
            let column = block * crate::quant::BLOCK;
            for index in 0..crate::quant::BLOCK {
                let weight = weights[offset + crate::quant::SCALE_BYTES + index] as i8 as f32;
                accumulator += weight * input[column + index];
            }
            sum += accumulator * scale;
        }
        *slot = sum;
    }
    Ok(())
}

/// Check every length the `bfloat16` kernel indexes.
fn validate(weights: &[u8], rows: usize, columns: usize, input: &[f32], out: &[f32]) -> Result<()> {
    if rows == 0 || columns == 0 {
        return Err(Error::new("matvec: an empty matrix has no product"));
    }
    if input.len() != columns {
        return Err(Error::new(
            "matvec: input length disagrees with the columns",
        ));
    }
    if out.len() < rows {
        return Err(Error::new("matvec: output is shorter than the rows"));
    }
    let needed = rows
        .checked_mul(columns)
        .and_then(|elements| elements.checked_mul(2))
        .ok_or_else(|| Error::new("matvec: matrix byte length overflow"))?;
    if weights.len() < needed {
        return Err(Error::new("matvec: weights are shorter than the matrix"));
    }
    Ok(())
}

/// Check every length and the block alignment the `Q4B32` kernel indexes.
fn validate_q4(
    weights: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
    out: &[f32],
) -> Result<()> {
    if rows == 0 || columns == 0 {
        return Err(Error::new("matvec: an empty matrix has no product"));
    }
    if input.len() != columns {
        return Err(Error::new(
            "matvec: input length disagrees with the columns",
        ));
    }
    if out.len() < rows {
        return Err(Error::new("matvec: output is shorter than the rows"));
    }
    if !columns.is_multiple_of(crate::quant::BLOCK) {
        return Err(Error::new(format!(
            "matvec: {columns} columns is not a multiple of the {} weight block",
            crate::quant::BLOCK
        )));
    }
    let needed = crate::quant::matrix_bytes(rows, columns)?;
    if weights.len() < needed {
        return Err(Error::new(
            "matvec: quantized weights are shorter than the matrix",
        ));
    }
    Ok(())
}

/// Check every length and the block alignment the `Q8B32` kernel indexes.
fn validate_q8(
    weights: &[u8],
    rows: usize,
    columns: usize,
    input: &[f32],
    out: &[f32],
) -> Result<()> {
    if rows == 0 || columns == 0 {
        return Err(Error::new("matvec: an empty matrix has no product"));
    }
    if input.len() != columns {
        return Err(Error::new(
            "matvec: input length disagrees with the columns",
        ));
    }
    if out.len() < rows {
        return Err(Error::new("matvec: output is shorter than the rows"));
    }
    if !columns.is_multiple_of(crate::quant::BLOCK) {
        return Err(Error::new(format!(
            "matvec: {columns} columns is not a multiple of the {} weight block",
            crate::quant::BLOCK
        )));
    }
    let needed = crate::quant::q8_matrix_bytes(rows, columns)?;
    if weights.len() < needed {
        return Err(Error::new(
            "matvec: quantized weights are shorter than the matrix",
        ));
    }
    Ok(())
}

/// The AVX2 and FMA kernel.
#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{COLUMN_TILE, ROW_TILE};
    use core::arch::x86_64::{
        __m128i, __m256, _mm256_castps256_ps128, _mm256_castsi256_ps, _mm256_cvtepu16_epi32,
        _mm256_extractf128_ps, _mm256_fmadd_ps, _mm256_loadu_ps, _mm256_setzero_ps,
        _mm256_slli_epi32, _mm_add_ps, _mm_add_ss, _mm_cvtss_f32, _mm_loadu_si128, _mm_movehdup_ps,
        _mm_movehl_ps,
    };

    /// # Safety
    ///
    /// The caller must have validated the lengths and must run on a processor
    /// that reports `avx2` and `fma`.
    #[target_feature(enable = "avx2,fma")]
    pub(super) unsafe fn matvec(
        weights: &[u8],
        rows: usize,
        columns: usize,
        input: &[f32],
        out: &mut [f32],
    ) {
        // SAFETY: the caller validated every length, the pointers stay inside
        // the slices for each load, and the target features are present.
        unsafe {
            let aligned = columns & !7;
            let mut row_start = 0;
            while row_start + ROW_TILE <= rows {
                let mut accumulator = [_mm256_setzero_ps(); ROW_TILE];
                let mut column_start = 0;
                while column_start < aligned {
                    let end = (column_start + COLUMN_TILE).min(aligned);
                    let mut column = column_start;
                    while column < end {
                        let input_vector = _mm256_loadu_ps(input.as_ptr().add(column));
                        let mut lane = 0;
                        while lane < ROW_TILE {
                            let offset = ((row_start + lane) * columns + column) * 2;
                            let packed =
                                _mm_loadu_si128(weights.as_ptr().add(offset) as *const __m128i);
                            let widened = _mm256_cvtepu16_epi32(packed);
                            let shifted = _mm256_slli_epi32(widened, 16);
                            let values = _mm256_castsi256_ps(shifted);
                            accumulator[lane] =
                                _mm256_fmadd_ps(values, input_vector, accumulator[lane]);
                            lane += 1;
                        }
                        column += 8;
                    }
                    column_start = end;
                }
                for (lane, accumulator) in accumulator.iter().enumerate() {
                    let mut sum = horizontal(*accumulator);
                    for (column, &input_value) in
                        input.iter().enumerate().take(columns).skip(aligned)
                    {
                        let offset = ((row_start + lane) * columns + column) * 2;
                        let bits = u16::from_le_bytes([weights[offset], weights[offset + 1]]);
                        sum += f32::from_bits((bits as u32) << 16) * input_value;
                    }
                    out[row_start + lane] = sum;
                }
                row_start += ROW_TILE;
            }
            while row_start < rows {
                out[row_start] = dot_row(weights, row_start, columns, input);
                row_start += 1;
            }
        }
    }

    /// One row of the tail, after the four row tiles.
    ///
    /// # Safety
    ///
    /// The lengths are validated by `matvec` and the target features are
    /// present.
    #[target_feature(enable = "avx2,fma")]
    unsafe fn dot_row(weights: &[u8], row: usize, columns: usize, input: &[f32]) -> f32 {
        // SAFETY: the row and column ranges are inside the slices.
        unsafe {
            let aligned = columns & !7;
            let mut accumulator = _mm256_setzero_ps();
            let mut column = 0;
            while column < aligned {
                let offset = (row * columns + column) * 2;
                let packed = _mm_loadu_si128(weights.as_ptr().add(offset) as *const __m128i);
                let widened = _mm256_cvtepu16_epi32(packed);
                let shifted = _mm256_slli_epi32(widened, 16);
                let values = _mm256_castsi256_ps(shifted);
                let input_vector = _mm256_loadu_ps(input.as_ptr().add(column));
                accumulator = _mm256_fmadd_ps(values, input_vector, accumulator);
                column += 8;
            }
            let mut sum = horizontal(accumulator);
            for (column, &input_value) in input.iter().enumerate().take(columns).skip(aligned) {
                let offset = (row * columns + column) * 2;
                let bits = u16::from_le_bytes([weights[offset], weights[offset + 1]]);
                sum += f32::from_bits((bits as u32) << 16) * input_value;
            }
            sum
        }
    }

    /// Sum the eight lanes of one accumulator.
    ///
    /// # Safety
    ///
    /// The caller must run on a processor with AVX2.
    #[target_feature(enable = "avx2")]
    unsafe fn horizontal(value: __m256) -> f32 {
        let high = _mm256_extractf128_ps(value, 1);
        let low = _mm256_castps256_ps128(value);
        let mut sum = _mm_add_ps(low, high);
        let duplicated = _mm_movehdup_ps(sum);
        sum = _mm_add_ps(sum, duplicated);
        let moved = _mm_movehl_ps(sum, sum);
        sum = _mm_add_ss(sum, moved);
        _mm_cvtss_f32(sum)
    }
}

/// The SSE2 kernel, used when AVX2 and FMA are not available.
#[cfg(target_arch = "x86_64")]
mod sse2 {
    use super::{COLUMN_TILE, ROW_TILE};
    use core::arch::x86_64::{
        __m128, __m128i, _mm_add_ps, _mm_add_ss, _mm_castsi128_ps, _mm_cvtss_f32, _mm_loadl_epi64,
        _mm_loadu_ps, _mm_movehl_ps, _mm_mul_ps, _mm_setzero_ps, _mm_setzero_si128, _mm_shuffle_ps,
        _mm_slli_epi32, _mm_unpacklo_epi16,
    };

    /// # Safety
    ///
    /// The caller must have validated the lengths. SSE2 is part of the x86-64
    /// baseline.
    pub(super) unsafe fn matvec(
        weights: &[u8],
        rows: usize,
        columns: usize,
        input: &[f32],
        out: &mut [f32],
    ) {
        // SAFETY: the caller validated every length, and every load stays
        // inside the slices.
        unsafe {
            let zero = _mm_setzero_ps();
            let aligned = columns & !7;
            let mut row_start = 0;
            while row_start + ROW_TILE <= rows {
                let mut accumulator = [[zero; 2]; ROW_TILE];
                let mut column_start = 0;
                while column_start < aligned {
                    let end = (column_start + COLUMN_TILE).min(aligned);
                    let mut column = column_start;
                    while column < end {
                        let input_low = _mm_loadu_ps(input.as_ptr().add(column));
                        let input_high = _mm_loadu_ps(input.as_ptr().add(column + 4));
                        let mut lane = 0;
                        while lane < ROW_TILE {
                            let offset = ((row_start + lane) * columns + column) * 2;
                            let low = widen(&weights[offset..]);
                            let high = widen(&weights[offset + 8..]);
                            accumulator[lane][0] =
                                _mm_add_ps(accumulator[lane][0], _mm_mul_ps(low, input_low));
                            accumulator[lane][1] =
                                _mm_add_ps(accumulator[lane][1], _mm_mul_ps(high, input_high));
                            lane += 1;
                        }
                        column += 8;
                    }
                    column_start = end;
                }
                for (lane, accumulator) in accumulator.iter().enumerate() {
                    let mut sum = horizontal(_mm_add_ps(accumulator[0], accumulator[1]));
                    for (column, &input_value) in
                        input.iter().enumerate().take(columns).skip(aligned)
                    {
                        let offset = ((row_start + lane) * columns + column) * 2;
                        let bits = u16::from_le_bytes([weights[offset], weights[offset + 1]]);
                        sum += f32::from_bits((bits as u32) << 16) * input_value;
                    }
                    out[row_start + lane] = sum;
                }
                row_start += ROW_TILE;
            }
            while row_start < rows {
                let mut sum = 0.0f32;
                for (column, &input_value) in input.iter().enumerate().take(columns) {
                    let offset = (row_start * columns + column) * 2;
                    let bits = u16::from_le_bytes([weights[offset], weights[offset + 1]]);
                    sum += f32::from_bits((bits as u32) << 16) * input_value;
                }
                out[row_start] = sum;
                row_start += 1;
            }
        }
    }

    /// Widen four `bfloat16` values to `f32`.
    ///
    /// # Safety
    ///
    /// `bytes` must hold at least eight bytes.
    #[target_feature(enable = "sse2")]
    unsafe fn widen(bytes: &[u8]) -> __m128 {
        // SAFETY: the caller guarantees the eight bytes the load reads.
        unsafe {
            let packed: __m128i = _mm_loadl_epi64(bytes.as_ptr() as *const __m128i);
            let widened = _mm_unpacklo_epi16(packed, _mm_setzero_si128());
            let shifted = _mm_slli_epi32(widened, 16);
            _mm_castsi128_ps(shifted)
        }
    }

    /// Sum the four lanes of one accumulator.
    ///
    /// # Safety
    ///
    /// The caller must run on a processor with SSE2.
    #[target_feature(enable = "sse2")]
    unsafe fn horizontal(value: __m128) -> f32 {
        let moved = _mm_movehl_ps(value, value);
        let mut sum = _mm_add_ps(value, moved);
        let duplicated = _mm_shuffle_ps::<0x55>(sum, sum);
        sum = _mm_add_ss(sum, duplicated);
        _mm_cvtss_f32(sum)
    }
}

/// The NEON kernel.
#[cfg(target_arch = "aarch64")]
mod neon {
    use super::{COLUMN_TILE, ROW_TILE};
    use core::arch::aarch64::{
        float32x4_t, uint16x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1_u16,
        vld1q_f32, vmovl_u16, vreinterpretq_f32_u32, vshlq_n_u32,
    };

    /// # Safety
    ///
    /// The caller must have validated the lengths. NEON is part of the aarch64
    /// baseline.
    pub(super) unsafe fn matvec(
        weights: &[u8],
        rows: usize,
        columns: usize,
        input: &[f32],
        out: &mut [f32],
    ) {
        // SAFETY: the caller validated every length, and every load stays
        // inside the slices.
        unsafe {
            let zero = vdupq_n_f32(0.0);
            let aligned = columns & !7;
            let mut row_start = 0;
            while row_start + ROW_TILE <= rows {
                let mut accumulator = [[zero; 2]; ROW_TILE];
                let mut column_start = 0;
                while column_start < aligned {
                    let end = (column_start + COLUMN_TILE).min(aligned);
                    let mut column = column_start;
                    while column < end {
                        let input_low = vld1q_f32(input.as_ptr().add(column));
                        let input_high = vld1q_f32(input.as_ptr().add(column + 4));
                        let mut lane = 0;
                        while lane < ROW_TILE {
                            let offset = ((row_start + lane) * columns + column) * 2;
                            let low = widen(&weights[offset..]);
                            let high = widen(&weights[offset + 8..]);
                            accumulator[lane][0] = vfmaq_f32(accumulator[lane][0], low, input_low);
                            accumulator[lane][1] =
                                vfmaq_f32(accumulator[lane][1], high, input_high);
                            lane += 1;
                        }
                        column += 8;
                    }
                    column_start = end;
                }
                for (lane, accumulator) in accumulator.iter().enumerate() {
                    let mut sum = vaddvq_f32(vaddq_f32(accumulator[0], accumulator[1]));
                    for (column, &input_value) in
                        input.iter().enumerate().take(columns).skip(aligned)
                    {
                        let offset = ((row_start + lane) * columns + column) * 2;
                        let bits = u16::from_le_bytes([weights[offset], weights[offset + 1]]);
                        sum += f32::from_bits((bits as u32) << 16) * input_value;
                    }
                    out[row_start + lane] = sum;
                }
                row_start += ROW_TILE;
            }
            while row_start < rows {
                let mut sum = 0.0f32;
                for (column, &input_value) in input.iter().enumerate().take(columns) {
                    let offset = (row_start * columns + column) * 2;
                    let bits = u16::from_le_bytes([weights[offset], weights[offset + 1]]);
                    sum += f32::from_bits((bits as u32) << 16) * input_value;
                }
                out[row_start] = sum;
                row_start += 1;
            }
        }
    }

    /// Widen four `bfloat16` values to `f32`.
    ///
    /// # Safety
    ///
    /// `bytes` must hold at least eight bytes.
    unsafe fn widen(bytes: &[u8]) -> float32x4_t {
        // SAFETY: the caller guarantees the eight bytes the load reads.
        unsafe {
            let packed: uint16x4_t = vld1_u16(bytes.as_ptr() as *const u16);
            let widened = vmovl_u16(packed);
            let shifted = vshlq_n_u32::<16>(widened);
            vreinterpretq_f32_u32(shifted)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{matvec_bf16, matvec_q4, matvec_q8, matvec_scalar, COLUMN_TILE, ROW_TILE};
    use crate::quant::{
        f32_to_half, matrix_bytes, q8_matrix_bytes, BLOCK, BLOCK_BYTES, Q8_BLOCK_BYTES,
        SCALE_BYTES,
    };

    /// Deterministic weights in a small range, stored as `bfloat16`.
    fn weights(count: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9u64;
        let mut bytes = Vec::with_capacity(count * 2);
        for _ in 0..count {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let value = ((state >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0;
            let bits = value.to_bits();
            let rounded = bits.wrapping_add(0x7fff + ((bits >> 16) & 1));
            bytes.extend_from_slice(&(rounded >> 16).to_le_bytes());
        }
        bytes
    }

    fn input(columns: usize) -> Vec<f32> {
        (0..columns)
            .map(|index| ((index as f32) * 0.031 - 0.7).sin())
            .collect()
    }

    fn error(actual: &[f32], expected: &[f32]) -> f32 {
        actual
            .iter()
            .zip(expected)
            .map(|(left, right)| (left - right).abs())
            .fold(0.0f32, f32::max)
    }

    #[test]
    fn the_vector_path_agrees_with_the_scalar_path() {
        for rows in [1usize, 3, ROW_TILE, ROW_TILE + 1, 37] {
            for columns in [1usize, 7, 8, 31, 576, COLUMN_TILE + 5] {
                let weights = weights(rows * columns);
                let input = input(columns);
                let mut scalar = vec![0.0f32; rows];
                let mut vector = vec![0.0f32; rows];
                matvec_scalar(&weights, rows, columns, &input, &mut scalar);
                matvec_bf16(&weights, rows, columns, &input, &mut vector).expect("shapes agree");
                let tolerance = 1e-4 * columns as f32;
                assert!(
                    error(&vector, &scalar) <= tolerance,
                    "rows {rows} columns {columns} diff {}",
                    error(&vector, &scalar)
                );
            }
        }
    }

    #[test]
    fn known_products_are_exact() {
        // Every value is exact in bfloat16 and every product is exact, so the
        // expected sums are a fixed golden value for all three kernels. Eight
        // columns fill one vector step; five rows leave one row for the tail.
        let rows = [
            [1.0f32, 2.0, 3.0, 4.0, 1.0, 2.0, 3.0, 4.0],
            [-1.0, 0.0, 1.0, 0.5, -1.0, 0.0, 1.0, 0.5],
            [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            [2.0, 0.0, 0.0, 0.0, 2.0, 0.0, 0.0, 0.0],
        ];
        let input = [0.5f32, 1.0, 2.0, -1.0, 0.5, 1.0, 2.0, -1.0];
        let expected = [9.0f32, 2.0, -2.0, 5.0, 2.0];
        let mut weights = Vec::new();
        for row in rows {
            for value in row {
                let bits = value.to_bits();
                weights.extend_from_slice(&((bits >> 16) as u16).to_le_bytes());
            }
        }
        let mut scalar = [0.0f32; 5];
        let mut vector = [0.0f32; 5];
        matvec_scalar(&weights, 5, 8, &input, &mut scalar);
        matvec_bf16(&weights, 5, 8, &input, &mut vector).expect("shapes agree");
        assert_eq!(scalar, expected);
        assert_eq!(vector, expected);
    }

    #[test]
    fn a_matrix_of_ones_sums_the_input() {
        let columns = 19usize;
        let rows = 9usize;
        let one = 0x3f80u16.to_le_bytes();
        let mut weights = Vec::with_capacity(rows * columns * 2);
        for _ in 0..rows * columns {
            weights.extend_from_slice(&one);
        }
        let input: Vec<f32> = (0..columns).map(|index| index as f32).collect();
        let expected: f32 = input.iter().sum();
        let mut out = vec![0.0f32; rows];
        matvec_bf16(&weights, rows, columns, &input, &mut out).expect("shapes agree");
        for value in out {
            assert!((value - expected).abs() < 1e-3);
        }
    }

    #[test]
    fn rejects_a_short_output_and_a_short_matrix() {
        let input = [1.0f32, 2.0];
        let mut out = [0.0f32];
        assert!(matvec_bf16(&[0u8; 8], 2, 2, &input, &mut out).is_err());
        assert!(matvec_bf16(&[0u8; 4], 2, 2, &input, &mut out).is_err());
    }

    /// Pack one block from a binary16 scale and thirty-two quantized weights.
    fn q4_block(scale: f32, values: &[i32; BLOCK]) -> Vec<u8> {
        let mut block = vec![0u8; BLOCK_BYTES];
        block[..SCALE_BYTES].copy_from_slice(&f32_to_half(scale).to_le_bytes());
        for (index, &value) in values.iter().enumerate() {
            block[SCALE_BYTES + index / 2] |= ((value & 0x0f) as u8) << (4 * (index % 2));
        }
        block
    }

    #[test]
    fn the_quantized_kernel_matches_the_reference_product() {
        // Five rows of two blocks with an exact scale per block. The quantized
        // weights and the input are dyadic and every product and partial sum is
        // exact in `f32`, so the expected values below are the ones the
        // reference implementation (numpy, float64) gives for the dequantized
        // matrix. A lane or a nibble that is read in the wrong order moves one
        // of them.
        let columns = 2 * BLOCK;
        let rows = 5usize;
        let scales = [0.5f32, 1.5];
        let mut weights = Vec::with_capacity(matrix_bytes(rows, columns).unwrap());
        for row in 0..rows {
            for (block, &scale) in scales.iter().enumerate() {
                let mut values = [0i32; BLOCK];
                for (index, slot) in values.iter_mut().enumerate() {
                    *slot = ((row * 37 + (block * 32 + index) * 11) % 16) as i32 - 8;
                }
                weights.extend_from_slice(&q4_block(scale, &values));
            }
        }
        let mut input = [0.0f32; 2 * BLOCK];
        for (index, slot) in input.iter_mut().enumerate() {
            *slot = [0.25f32, -0.5, 0.75, 1.0, 1.25][index % 5];
        }
        let mut out = [0.0f32; 5];
        matvec_q4(&weights, rows, columns, &input, &mut out).unwrap();
        // The product computed in float64 from the dequantized weights.
        assert_eq!(out, [-24.0, -22.5, -21.0, -19.5, -2.0]);
    }

    #[test]
    fn the_quantized_kernel_agrees_with_the_expanded_product() {
        let rows = 3usize;
        let columns = 5 * BLOCK;
        let values: Vec<f32> = (0..rows * columns)
            .map(|index| ((index as f32) * 0.071 - 0.9).sin())
            .collect();
        let mut weights = vec![0u8; matrix_bytes(rows, columns).unwrap()];
        for (block, chunk) in values.as_chunks::<BLOCK>().0.iter().enumerate() {
            crate::quant::quantize_block(
                chunk,
                &mut weights[block * BLOCK_BYTES..(block + 1) * BLOCK_BYTES],
            )
            .unwrap();
        }
        let input: Vec<f32> = (0..columns)
            .map(|index| ((index as f32) * 0.13).cos())
            .collect();
        let mut out = vec![0.0f32; rows];
        matvec_q4(&weights, rows, columns, &input, &mut out).unwrap();
        let row_bytes = matrix_bytes(1, columns).unwrap();
        for (row, actual) in out.iter().enumerate() {
            // The expected value is the sum over the dequantized row, read
            // back through the format itself.
            let mut decoded = vec![0.0f32; columns];
            crate::quant::dequantize_row(
                &weights[row * row_bytes..(row + 1) * row_bytes],
                &mut decoded,
            )
            .unwrap();
            let expected: f64 = decoded
                .iter()
                .zip(&input)
                .map(|(weight, input)| *weight as f64 * *input as f64)
                .sum();
            assert!(
                (*actual as f64 - expected).abs() < 1e-4,
                "row {row}: {actual} against {expected}"
            );
        }
    }

    /// Pack one eight bit block from a binary16 scale and signed weights.
    fn q8_block(scale: f32, values: &[i32; BLOCK]) -> Vec<u8> {
        let mut block = vec![0u8; Q8_BLOCK_BYTES];
        block[..SCALE_BYTES].copy_from_slice(&f32_to_half(scale).to_le_bytes());
        for (index, &value) in values.iter().enumerate() {
            block[SCALE_BYTES + index] = (value as i8) as u8;
        }
        block
    }

    #[test]
    fn the_eight_bit_kernel_matches_the_reference_product() {
        // The same shape as the four bit golden: five rows, two blocks, one
        // exact scale per block. The expected values are the ones a float64
        // reference gives for the dequantized matrix.
        let columns = 2 * BLOCK;
        let rows = 5usize;
        let mut weights = Vec::with_capacity(q8_matrix_bytes(rows, columns).unwrap());
        for row in 0..rows {
            for (block, &scale) in [1.0f32, 2.0].iter().enumerate() {
                let mut values = [0i32; BLOCK];
                for (index, slot) in values.iter_mut().enumerate() {
                    *slot = ((row * 53 + (block * 32 + index) * 17) % 255) as i32 - 127;
                }
                weights.extend_from_slice(&q8_block(scale, &values));
            }
        }
        let mut input = [0.0f32; 2 * BLOCK];
        for (index, slot) in input.iter_mut().enumerate() {
            *slot = [0.25f32, -0.5, 0.75, 1.0, 1.25][index % 5];
        }
        let mut out = [0.0f32; 5];
        matvec_q8(&weights, rows, columns, &input, &mut out).unwrap();
        assert_eq!(out, [406.75, -1387.5, -121.75, 761.5, -777.75]);
    }

    #[test]
    fn the_eight_bit_kernel_agrees_with_the_expanded_product() {
        let rows = 3usize;
        let columns = 5 * BLOCK;
        let values: Vec<f32> = (0..rows * columns)
            .map(|index| ((index as f32) * 0.067 - 0.4).sin())
            .collect();
        let mut weights = vec![0u8; q8_matrix_bytes(rows, columns).unwrap()];
        for (block, chunk) in values.as_chunks::<BLOCK>().0.iter().enumerate() {
            crate::quant::quantize_block_q8(
                chunk,
                &mut weights[block * Q8_BLOCK_BYTES..(block + 1) * Q8_BLOCK_BYTES],
            )
            .unwrap();
        }
        let input: Vec<f32> = (0..columns)
            .map(|index| ((index as f32) * 0.11).cos())
            .collect();
        let mut out = vec![0.0f32; rows];
        matvec_q8(&weights, rows, columns, &input, &mut out).unwrap();
        let row_bytes = q8_matrix_bytes(1, columns).unwrap();
        for (row, actual) in out.iter().enumerate() {
            let mut decoded = vec![0.0f32; columns];
            crate::quant::dequantize_row_q8(
                &weights[row * row_bytes..(row + 1) * row_bytes],
                &mut decoded,
            )
            .unwrap();
            let expected: f64 = decoded
                .iter()
                .zip(&input)
                .map(|(weight, input)| *weight as f64 * *input as f64)
                .sum();
            assert!(
                (*actual as f64 - expected).abs() < 1e-3,
                "row {row}: {actual} against {expected}"
            );
        }
    }

    #[test]
    fn the_quantized_kernel_rejects_a_bad_shape() {
        let input = [1.0f32; BLOCK];
        let mut out = [0.0f32; 1];
        // Columns that are not a multiple of the block.
        assert!(matvec_q4(&[0u8; 64], 1, BLOCK - 1, &input[..BLOCK - 1], &mut out).is_err());
        // A matrix shorter than its shape.
        assert!(matvec_q4(&[0u8; BLOCK_BYTES - 1], 1, BLOCK, &input, &mut out).is_err());
        // An input that disagrees with the columns.
        assert!(matvec_q4(&[0u8; BLOCK_BYTES], 1, BLOCK, &input[..1], &mut out).is_err());
    }
}
