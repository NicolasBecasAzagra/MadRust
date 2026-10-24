// SPDX-License-Identifier: GPL-2.0-only
//! The tensor directory of a `safetensors` file.
//!
//! A `safetensors` file starts with an eight byte little endian header length,
//! then that many bytes of JSON, then the raw tensor data. Each top level key
//! of the JSON object is a tensor name; its value carries `dtype`, `shape` and
//! `data_offsets`, where the offsets are relative to the first byte after the
//! header. The optional `__metadata__` key is not a tensor.
//!
//! The JSON is read by the scanner in [`crate::json`], which is written for
//! this header: it walks the top level object, decodes the three fields a
//! tensor needs, and skips any other field without building it. There is no
//! dependency on a JSON crate.
//!
//! Names and shape dimensions are stored flat. A real model carries hundreds of
//! tensors, and one heap allocation per name would exhaust a kernel's fixed
//! user mapping table before the first token; the directory instead keeps all
//! names in one string and every shape in a small fixed array.

use crate::error::{Error, Result};
use crate::json::Scanner;

/// Largest tensor rank the decoder accepts.
///
/// The decoder uses 1-D norm weights and 2-D matrices. A larger rank is
/// rejected instead of silently truncated.
const MAX_DIMS: usize = 2;

/// Byte order of the tensor data this runtime understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DType {
    /// Brain floating point: the top 16 bits of an `f32`.
    Bf16,
    /// Single precision floating point.
    F32,
    /// A type the reader keeps in the directory but the runtime must not use.
    Other,
}

impl DType {
    /// Bytes per element, or `None` for a type the runtime cannot read.
    pub fn element_size(self) -> Option<usize> {
        match self {
            Self::Bf16 => Some(2),
            Self::F32 => Some(4),
            Self::Other => None,
        }
    }

    /// Parse the `dtype` field.
    fn parse(name: &str) -> Self {
        match name {
            "BF16" => Self::Bf16,
            "F32" => Self::F32,
            _ => Self::Other,
        }
    }

    /// Name of the type as the header spells it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Bf16 => "BF16",
            Self::F32 => "F32",
            Self::Other => "unsupported",
        }
    }
}

/// One tensor as the header describes it.
#[derive(Clone, Copy, Debug)]
pub struct TensorInfo {
    name_start: usize,
    name_end: usize,
    /// Element type.
    pub dtype: DType,
    shape: [usize; MAX_DIMS],
    rank: usize,
    /// First data byte, relative to the end of the header.
    pub start: usize,
    /// One past the last data byte, relative to the end of the header.
    pub end: usize,
}

impl TensorInfo {
    /// Dimensions, outermost first.
    pub fn shape(&self) -> &[usize] {
        &self.shape[..self.rank]
    }

    /// Number of elements the shape describes.
    pub fn element_count(&self) -> usize {
        self.shape().iter().copied().product()
    }

    /// Number of bytes of tensor data.
    pub fn byte_len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

/// The parsed tensor directory.
#[derive(Debug)]
pub struct Header {
    names: String,
    tensors: Vec<TensorInfo>,
}

impl Header {
    /// Name of one tensor.
    pub fn name(&self, tensor: &TensorInfo) -> &str {
        &self.names[tensor.name_start..tensor.name_end]
    }

    /// Find one tensor by name.
    pub fn find(&self, name: &str) -> Option<&TensorInfo> {
        self.tensors.iter().find(|tensor| self.name(tensor) == name)
    }

    /// Every tensor in the file.
    pub fn tensors(&self) -> &[TensorInfo] {
        &self.tensors
    }
}

/// Parse the JSON header.
///
/// `data_len` is the number of data bytes after the header, so an offset that
/// points outside the file is rejected here instead of at the first read.
pub fn parse(bytes: &[u8], data_len: usize) -> Result<Header> {
    let mut scanner = Scanner::new(bytes);
    let mut names = String::with_capacity(bytes.len());
    let mut scratch = Vec::with_capacity(64);
    let mut shape = Vec::new();
    let mut offsets = Vec::new();
    // One allocation for the directory. A model with a few hundred tensors
    // stays well inside this estimate, so the vector does not grow through
    // `realloc` on a kernel whose `mremap` is absent.
    let mut tensors = Vec::with_capacity(bytes.len() / 16 + 1);

    scanner.skip_whitespace();
    scanner.expect(b'{')?;
    scanner.skip_whitespace();
    if scanner.peek() == Some(b'}') {
        scanner.expect(b'}')?;
        return Ok(Header { names, tensors });
    }

    loop {
        scanner.skip_whitespace();
        let name_start = names.len();
        scanner.parse_string_into(&mut scratch)?;
        let name = core::str::from_utf8(&scratch)
            .map_err(|_| Error::new("safetensors: tensor name is not valid UTF-8"))?;
        names.push_str(name);
        let name_end = names.len();

        scanner.skip_whitespace();
        scanner.expect(b':')?;
        if name == "__metadata__" {
            scanner.skip_value()?;
        } else {
            tensors.push(parse_tensor(
                &mut scanner,
                &names[name_start..name_end],
                data_len,
                name_start,
                name_end,
                &mut shape,
                &mut offsets,
            )?);
        }
        scanner.skip_whitespace();
        match scanner.bump()? {
            b',' => continue,
            b'}' => break,
            _ => return Err(Error::new("safetensors: expected ',' or '}' in the header")),
        }
    }

    if !scanner.at_end() {
        return Err(Error::new("safetensors: trailing bytes after the header"));
    }
    Ok(Header { names, tensors })
}

/// Parse one tensor object.
#[allow(clippy::too_many_arguments)]
fn parse_tensor(
    scanner: &mut Scanner<'_>,
    name: &str,
    data_len: usize,
    name_start: usize,
    name_end: usize,
    shape_scratch: &mut Vec<usize>,
    offsets_scratch: &mut Vec<usize>,
) -> Result<TensorInfo> {
    scanner.skip_whitespace();
    scanner.expect(b'{')?;
    scanner.skip_whitespace();
    if scanner.peek() == Some(b'}') {
        return Err(Error::new(format!(
            "safetensors: tensor {name} has no fields"
        )));
    }

    let mut dtype = None;
    let mut dimensions = [0usize; MAX_DIMS];
    let mut rank = None;
    let mut offsets = [0usize; 2];
    let mut has_offsets = false;
    loop {
        scanner.skip_whitespace();
        let field = scanner.parse_string()?;
        scanner.skip_whitespace();
        scanner.expect(b':')?;
        scanner.skip_whitespace();
        match field.as_str() {
            "dtype" => dtype = Some(DType::parse(&scanner.parse_string()?)),
            "shape" => {
                scanner.parse_usize_array_into(shape_scratch)?;
                if shape_scratch.len() > MAX_DIMS {
                    return Err(Error::new(format!(
                        "safetensors: {name} has an unsupported shape"
                    )));
                }
                dimensions[..shape_scratch.len()].copy_from_slice(shape_scratch);
                rank = Some(shape_scratch.len());
            }
            "data_offsets" => {
                scanner.parse_usize_array_into(offsets_scratch)?;
                if offsets_scratch.len() != 2 {
                    return Err(Error::new(format!(
                        "safetensors: {name} has invalid data offsets"
                    )));
                }
                offsets.copy_from_slice(offsets_scratch);
                has_offsets = true;
            }
            _ => scanner.skip_value()?,
        }
        scanner.skip_whitespace();
        match scanner.bump()? {
            b',' => continue,
            b'}' => break,
            _ => {
                return Err(Error::new(format!(
                    "safetensors: expected ',' or '}}' in tensor {name}"
                )))
            }
        }
    }

    let dtype = dtype.ok_or_else(|| Error::new(format!("safetensors: {name} has no dtype")))?;
    let rank = rank.ok_or_else(|| Error::new(format!("safetensors: {name} has no shape")))?;
    if rank == 0 || dimensions[..rank].contains(&0) {
        return Err(Error::new(format!(
            "safetensors: {name} has an unsupported shape"
        )));
    }
    if !has_offsets || offsets[0] > offsets[1] || offsets[1] > data_len {
        return Err(Error::new(format!(
            "safetensors: {name} has invalid data offsets"
        )));
    }

    Ok(TensorInfo {
        name_start,
        name_end,
        dtype,
        shape: dimensions,
        rank,
        start: offsets[0],
        end: offsets[1],
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, DType};

    #[test]
    fn parses_a_tensor_and_skips_metadata() {
        let header = br#"{
            "__metadata__": {"format": "pt"},
            "a": {"dtype": "BF16", "shape": [2, 3], "data_offsets": [0, 12]},
            "b": {"dtype": "F32", "shape": [1], "data_offsets": [12, 16]}
        }"#;
        let parsed = parse(header, 16).unwrap();
        let a = parsed.find("a").unwrap();
        assert_eq!(a.dtype, DType::Bf16);
        assert_eq!(a.shape(), [2, 3]);
        assert_eq!(a.element_count(), 6);
        assert_eq!(a.byte_len(), 12);
        assert_eq!(parsed.name(a), "a");
        let b = parsed.find("b").unwrap();
        assert_eq!(b.dtype, DType::F32);
        assert_eq!(parsed.name(b), "b");
        assert!(parsed.find("__metadata__").is_none());
    }

    #[test]
    fn rejects_offsets_past_the_data() {
        let header = br#"{"a": {"dtype": "BF16", "shape": [1], "data_offsets": [0, 2]}}"#;
        assert!(parse(header, 1).is_err());
    }

    #[test]
    fn keeps_an_unknown_dtype_in_the_directory() {
        let header = br#"{"a": {"dtype": "I64", "shape": [1], "data_offsets": [0, 8]}}"#;
        let parsed = parse(header, 8).unwrap();
        assert_eq!(parsed.find("a").unwrap().dtype, DType::Other);
    }

    #[test]
    fn rejects_a_rank_the_decoder_does_not_use() {
        let header = br#"{"a": {"dtype": "BF16", "shape": [1, 1, 1], "data_offsets": [0, 2]}}"#;
        assert!(parse(header, 2).is_err());
    }
}
