// SPDX-License-Identifier: GPL-2.0-only
//! The pure kernels of the forward pass.
//!
//! These functions have no I/O and no allocator, so they are host tested and
//! the model code only has to wire them together. Every length is checked and
//! returned as an error instead of indexing past a slice: the runtime path must
//! not panic on a model whose shapes disagree with the config.

use crate::error::{Error, Result};

/// RMSNorm without bias: `x * rsqrt(mean(x * x) + eps) * weight`.
pub fn rms_norm(x: &[f32], weight: &[f32], eps: f32, out: &mut [f32]) -> Result<()> {
    if x.len() != weight.len() || x.len() != out.len() {
        return Err(Error::new("math: rms_norm length mismatch"));
    }
    let mut sum = 0.0f32;
    for &value in x {
        sum += value * value;
    }
    let scale = 1.0 / (sum / x.len() as f32 + eps).sqrt();
    for ((slot, &value), &gain) in out.iter_mut().zip(x).zip(weight) {
        *slot = value * scale * gain;
    }
    Ok(())
}

/// SiLU: `z / (1 + exp(-z))`.
pub fn silu(z: f32) -> f32 {
    z / (1.0 + (-z).exp())
}

/// Softmax in place, with the maximum subtracted for stability.
pub fn softmax(values: &mut [f32]) -> Result<()> {
    if values.is_empty() {
        return Err(Error::new("math: softmax of an empty slice"));
    }
    let mut maximum = values[0];
    for &value in &values[1..] {
        if value > maximum {
            maximum = value;
        }
    }
    let mut sum = 0.0f32;
    for value in values.iter_mut() {
        *value = (*value - maximum).exp();
        sum += *value;
    }
    if !sum.is_finite() || sum <= 0.0 {
        return Err(Error::new("math: softmax has no finite mass"));
    }
    for value in values.iter_mut() {
        *value /= sum;
    }
    Ok(())
}

/// Index of the largest value; the first index wins a tie.
pub fn argmax(values: &[f32]) -> Option<usize> {
    let mut best = 0usize;
    let mut maximum = *values.first()?;
    for (index, &value) in values.iter().enumerate().skip(1) {
        if value > maximum {
            maximum = value;
            best = index;
        }
    }
    Some(best)
}

/// Apply RoPE to one head vector at `position`.
///
/// The pairs are `(2i, 2i + 1)` when `interleaved` is true and `(i, i + d/2)`
/// otherwise. The reference model uses the split in half form; both are
/// supported because the pair layout is a property of the checkpoint, not of
/// the architecture.
pub fn rope(vector: &mut [f32], position: usize, theta: f32, interleaved: bool) -> Result<()> {
    let dimension = vector.len();
    if dimension == 0 || dimension % 2 != 0 {
        return Err(Error::new(
            "math: RoPE needs an even, non-zero head dimension",
        ));
    }
    let half = dimension / 2;
    for index in 0..half {
        let (first, second) = if interleaved {
            (2 * index, 2 * index + 1)
        } else {
            (index, index + half)
        };
        let exponent = -2.0 * index as f32 / dimension as f32;
        let angle = position as f32 * theta.powf(exponent);
        let (sin, cos) = angle.sin_cos();
        let a = vector[first];
        let b = vector[second];
        vector[first] = a * cos - b * sin;
        vector[second] = a * sin + b * cos;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{argmax, rms_norm, rope, silu, softmax};

    #[test]
    fn rms_norm_scales_by_the_root_mean_square() {
        let x = [3.0, 4.0];
        let weight = [1.0, 2.0];
        let mut out = [0.0; 2];
        rms_norm(&x, &weight, 0.0, &mut out).unwrap();
        let scale = 1.0 / (12.5f32).sqrt();
        assert!((out[0] - 3.0 * scale).abs() < 1e-6);
        assert!((out[1] - 8.0 * scale).abs() < 1e-6);
    }

    #[test]
    fn rope_at_position_zero_is_the_identity() {
        let mut split = [1.0, 2.0, 3.0, 4.0];
        rope(&mut split, 0, 10_000.0, false).unwrap();
        assert_eq!(split, [1.0, 2.0, 3.0, 4.0]);
        let mut interleaved = [1.0, 2.0, 3.0, 4.0];
        rope(&mut interleaved, 0, 10_000.0, true).unwrap();
        assert_eq!(interleaved, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn rope_uses_the_pair_order_it_is_given() {
        let mut split = [1.0, 0.0, 0.0, 0.0];
        rope(&mut split, 1, 10_000.0, false).unwrap();
        let (sin, cos) = 1.0f32.sin_cos();
        assert!((split[0] - cos).abs() < 1e-6);
        assert!((split[2] - sin).abs() < 1e-6);
        assert_eq!(split[1], 0.0);
        assert_eq!(split[3], 0.0);

        let mut interleaved = [1.0, 0.0, 0.0, 0.0];
        rope(&mut interleaved, 1, 10_000.0, true).unwrap();
        assert!((interleaved[0] - cos).abs() < 1e-6);
        assert!((interleaved[1] - sin).abs() < 1e-6);
        assert_eq!(interleaved[2], 0.0);
    }

    #[test]
    fn softmax_has_unit_mass_and_argmax_takes_the_first_tie() {
        let mut values = [1.0, 1.0, 3.0];
        softmax(&mut values).unwrap();
        assert!((values.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert_eq!(argmax(&[2.0, 2.0, 1.0]), Some(0));
        assert_eq!(argmax(&[]), None);
    }

    #[test]
    fn silu_is_the_expected_ratio() {
        let z = 2.0f32;
        assert!((silu(z) - z / (1.0 + (-z).exp())).abs() < 1e-6);
    }
}
