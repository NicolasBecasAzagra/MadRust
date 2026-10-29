// SPDX-License-Identifier: GPL-2.0-only
//! The chunked transfer coding: the size line, the data and the end.
//!
//! A body with no announced length arrives as a sequence of chunks. Each chunk
//! is a line of hexadecimal size, the bytes of the chunk, and a CRLF; a chunk
//! of size zero ends the body, and the trailer fields that follow it end with
//! an empty line:
//!
//! ```text
//! 4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n
//! ```
//!
//! [`decode`] reads such a sequence from bytes that already arrived and writes
//! the body without the encoding into a second buffer. It reports that the
//! encoding is not complete yet instead of guessing, so a caller that reads
//! from a socket can call it again with more bytes. A size line that is not
//! hexadecimal and a chunk that is not followed by CRLF are errors with the
//! offset that was wrong.

use crate::{is_token_byte, is_value_byte, split_line, Error};

/// Largest size line this decoder accepts, extensions included.
const MAX_SIZE_LINE: usize = 64;

/// Decode a chunked body.
///
/// Returns `Ok(None)` when the bytes hold only the beginning of an encoding,
/// and `Ok(Some((consumed, produced)))` when the last chunk and its trailers
/// are in hand. `consumed` is the number of encoded bytes the body used, so a
/// caller can tell the bytes of a next message from the bytes of this one.
/// `produced` is the length written to `output`.
///
/// A body that does not fit `output` is [`Error::BufferFull`]; nothing is
/// written before the whole encoding is in hand, so a failed call leaves the
/// output as it was.
pub fn decode(input: &[u8], output: &mut [u8]) -> Result<Option<(usize, usize)>, Error> {
    let mut position = 0;
    let mut produced = 0usize;
    loop {
        let Some((size, next)) = size_line(input, position)? else {
            return Ok(None);
        };
        position = next;
        if size == 0 {
            let Some(end) = trailers(input, position)? else {
                return Ok(None);
            };
            position = end;
            break;
        }
        let Some(data_end) = position.checked_add(size) else {
            return Err(Error::ChunkSizeInvalid { offset: position });
        };
        let Some(delimiter) = data_end.checked_add(2) else {
            return Err(Error::ChunkSizeInvalid { offset: position });
        };
        if delimiter > input.len() {
            return Ok(None);
        }
        if &input[data_end..delimiter] != b"\r\n" {
            return Err(Error::ChunkDelimiter { offset: data_end });
        }
        position = delimiter;
        produced = produced
            .checked_add(size)
            .ok_or(Error::ChunkSizeInvalid { offset: position })?;
    }
    if produced > output.len() {
        return Err(Error::BufferFull {
            capacity: output.len(),
        });
    }
    // A second pass copies the data, now that the size of the body is known
    // and the output buffer is proven to hold it.
    let mut read = 0;
    let mut written = 0;
    loop {
        let (size, next) = size_line(input, read)?.ok_or(Error::Incomplete)?;
        read = next;
        if size == 0 {
            break;
        }
        output[written..written + size].copy_from_slice(&input[read..read + size]);
        written += size;
        read += size + 2;
    }
    Ok(Some((position, produced)))
}

/// Read the size line of one chunk: its value and the offset after its CRLF.
fn size_line(input: &[u8], start: usize) -> Result<Option<(usize, usize)>, Error> {
    let Some((end, next)) = split_line(input, start) else {
        let remaining = input.len().saturating_sub(start);
        if remaining > MAX_SIZE_LINE {
            return Err(Error::ChunkSizeInvalid { offset: start });
        }
        return Ok(None);
    };
    if end - start > MAX_SIZE_LINE {
        return Err(Error::ChunkSizeInvalid { offset: start });
    }
    let line = &input[start..end];
    let digits_end = line
        .iter()
        .position(|&byte| byte == b';')
        .unwrap_or(line.len());
    let digits = &line[..digits_end];
    if digits.is_empty() {
        return Err(Error::ChunkSizeInvalid { offset: start });
    }
    let mut size = 0usize;
    for &byte in digits {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(Error::ChunkSizeInvalid { offset: start }),
        };
        size = size
            .checked_mul(16)
            .and_then(|size| size.checked_add(usize::from(digit)))
            .ok_or(Error::ChunkSizeInvalid { offset: start })?;
    }
    Ok(Some((size, next)))
}

/// Read the trailer fields, and return the offset after their empty line.
///
/// The fields are not kept: a client of an API reads the body, and a trailer
/// that mattered would be announced in the headers. They are still validated,
/// so a broken body is refused with the line that is wrong.
fn trailers(input: &[u8], start: usize) -> Result<Option<usize>, Error> {
    let mut position = start;
    loop {
        let Some((end, next)) = split_line(input, position) else {
            return Ok(None);
        };
        if end == position {
            return Ok(Some(next));
        }
        let line = &input[position..end];
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            return Err(Error::TrailerInvalid { offset: position });
        };
        if colon == 0 || !line[..colon].iter().all(|&byte| is_token_byte(byte)) {
            return Err(Error::TrailerInvalid { offset: position });
        }
        if !line[colon + 1..].iter().all(|&byte| is_value_byte(byte)) {
            return Err(Error::TrailerInvalid { offset: position });
        }
        position = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_chunk_of_four_bytes_decodes_to_its_data() {
        let mut output = [0u8; 8];
        let decoded =
            decode(b"4\r\nWiki\r\n0\r\n\r\n", &mut output).expect("the encoding is valid");
        assert_eq!(decoded, Some((14, 4)));
        assert_eq!(&output[..4], b"Wiki");
    }

    #[test]
    fn several_chunks_of_different_case_decode_in_order() {
        let mut output = [0u8; 16];
        let decoded = decode(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n", &mut output)
            .expect("the encoding is valid");
        assert_eq!(decoded, Some((24, 9)));
        assert_eq!(&output[..9], b"Wikipedia");
        let mut upper = [0u8; 10];
        assert_eq!(
            decode(b"A\r\n0123456789\r\n0\r\n\r\n", &mut upper),
            Ok(Some((20, 10)))
        );
        assert_eq!(&upper[..10], b"0123456789");
    }

    #[test]
    fn an_extension_of_the_size_line_is_ignored() {
        let mut output = [0u8; 4];
        let decoded = decode(b"4;name=value\r\nWiki\r\n0\r\n\r\n", &mut output)
            .expect("the extension is legal");
        assert_eq!(decoded, Some((25, 4)));
        assert_eq!(&output[..4], b"Wiki");
    }

    #[test]
    fn a_trailer_field_after_the_last_chunk_is_read_and_dropped() {
        let mut output = [0u8; 4];
        let decoded = decode(b"4\r\nWiki\r\n0\r\nx-sum: 1\r\n\r\n", &mut output)
            .expect("the trailer is legal");
        assert_eq!(decoded, Some((24, 4)));
    }

    #[test]
    fn an_incomplete_encoding_is_not_a_message_yet() {
        let mut output = [0u8; 8];
        for prefix in [
            b"".as_slice(),
            b"4",
            b"4\r",
            b"4\r\n",
            b"4\r\nWi",
            b"4\r\nWiki\r",
            b"4\r\nWiki\r\n",
            b"4\r\nWiki\r\n0\r\n",
        ] {
            assert_eq!(
                decode(prefix, &mut output),
                Ok(None),
                "the prefix {prefix:?} is not complete"
            );
        }
    }

    #[test]
    fn a_bad_size_line_says_where() {
        let mut output = [0u8; 8];
        assert_eq!(
            decode(b"zz\r\n", &mut output),
            Err(Error::ChunkSizeInvalid { offset: 0 })
        );
        assert_eq!(
            decode(b"\r\n", &mut output),
            Err(Error::ChunkSizeInvalid { offset: 0 })
        );
    }

    #[test]
    fn a_chunk_without_its_crlf_says_where() {
        let mut output = [0u8; 8];
        assert_eq!(
            decode(b"4\r\nWikiXX", &mut output),
            Err(Error::ChunkDelimiter { offset: 7 })
        );
    }

    #[test]
    fn a_body_larger_than_the_output_is_refused() {
        let mut output = [0u8; 2];
        assert_eq!(
            decode(b"4\r\nWiki\r\n0\r\n\r\n", &mut output),
            Err(Error::BufferFull { capacity: 2 })
        );
    }

    #[test]
    fn a_zero_length_body_is_complete_with_no_data() {
        let mut output = [0u8; 0];
        assert_eq!(decode(b"0\r\n\r\n", &mut output), Ok(Some((5, 0))));
    }
}
