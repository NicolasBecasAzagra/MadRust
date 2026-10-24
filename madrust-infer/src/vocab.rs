// SPDX-License-Identifier: GPL-2.0-only
//! The vocabulary and the byte level decoder.
//!
//! The vocabulary file is a flat JSON object whose keys are token strings and
//! whose values are their ids, for example `{"\u0120the": 260, "!": 1}`. The
//! runtime builds the inverse map, id to token string, while it reads the file:
//! the decoder needs the string for an id, not the id for a string.
//!
//! Decoding maps every character of a token string back to a byte with the
//! byte-to-unicode table the GPT-2 style byte level tokenizers use, so the
//! original bytes are recovered without a merges file. Merges and a
//! `tokenizer.json` are only needed to encode; this runtime receives already
//! encoded prompt ids from the binary and only decodes.
//!
//! The scanner is the one in [`crate::json`], written for the flat object this
//! file is. A character outside the byte table is written as its own UTF-8
//! bytes and counted in [`Decoded::outside_alphabet`], which keeps a special
//! token visible and makes the fallback explicit in the log.
//!
//! Token strings live in one buffer with `(start, end)` offsets. A real
//! vocabulary has tens of thousands of entries; one allocation per string would
//! exhaust a kernel's fixed user mapping table long before the model runs.

use crate::error::{Error, Result};
use crate::json::Scanner;

/// The vocabulary, indexed by token id.
#[derive(Debug, Default)]
pub struct Vocab {
    text: String,
    tokens: Vec<Option<(usize, usize)>>,
}

impl Vocab {
    /// Parse a flat token string to token id object and invert it.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut scanner = Scanner::new(bytes);
        let mut text = String::with_capacity(bytes.len());
        let mut tokens: Vec<Option<(usize, usize)>> = Vec::with_capacity(bytes.len() / 4 + 1);
        let mut scratch = Vec::with_capacity(256);

        scanner.skip_whitespace();
        scanner.expect(b'{')?;
        scanner.skip_whitespace();
        if scanner.peek() == Some(b'}') {
            scanner.expect(b'}')?;
            return Ok(Self { text, tokens });
        }

        loop {
            scanner.skip_whitespace();
            scanner.parse_string_into(&mut scratch)?;
            let token = core::str::from_utf8(&scratch)
                .map_err(|_| Error::new("vocab: token is not valid UTF-8"))?;
            scanner.skip_whitespace();
            scanner.expect(b':')?;
            scanner.skip_whitespace();
            let id = scanner.parse_usize()?;
            let start = text.len();
            text.push_str(token);
            if tokens.len() <= id {
                tokens.resize(id + 1, None);
            }
            if tokens[id].is_some() {
                return Err(Error::new(format!("vocab: duplicate token id {id}")));
            }
            tokens[id] = Some((start, text.len()));
            scanner.skip_whitespace();
            match scanner.bump()? {
                b',' => continue,
                b'}' => break,
                _ => return Err(Error::new("vocab: expected ',' or '}'")),
            }
        }
        if !scanner.at_end() {
            return Err(Error::new("vocab: trailing bytes after the object"));
        }
        Ok(Self { text, tokens })
    }

    /// `true` when every id in `0..size` has a token string.
    ///
    /// The inverse map must be complete for the model vocabulary: a missing id
    /// would turn into an "unknown token" only when the model samples it.
    pub fn covers(&self, size: usize) -> bool {
        self.tokens.len() == size && self.tokens.iter().all(Option::is_some)
    }

    /// The token string of one id.
    pub fn token(&self, id: u32) -> Option<&str> {
        let (start, end) = self.tokens.get(id as usize).copied().flatten()?;
        self.text.get(start..end)
    }

    /// Decode token ids into the original bytes.
    ///
    /// A character in the byte-to-unicode table contributes its byte. A
    /// character outside it is not part of the byte-level alphabet: it is
    /// written as its own UTF-8 bytes and counted, so the runtime can report
    /// the fallback instead of applying it silently.
    pub fn decode(&self, ids: &[u32]) -> Result<Decoded> {
        let mut bytes = Vec::new();
        let mut outside_alphabet = 0usize;
        for &id in ids {
            let token = self
                .token(id)
                .ok_or_else(|| Error::new(format!("vocab: unknown token id {id}")))?;
            for character in token.chars() {
                match unicode_to_byte(character) {
                    Some(byte) => bytes.push(byte),
                    None => {
                        outside_alphabet += 1;
                        let mut buffer = [0u8; 4];
                        bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                    }
                }
            }
        }
        Ok(Decoded {
            bytes,
            outside_alphabet,
        })
    }
}

/// The bytes of a decoded token sequence.
#[derive(Debug)]
pub struct Decoded {
    /// Raw bytes, ready to be interpreted as UTF-8 text.
    pub bytes: Vec<u8>,
    /// Characters that were not in the byte-level alphabet and were written
    /// as their own UTF-8 bytes.
    pub outside_alphabet: usize,
}

/// `true` when the byte is printed as itself in the GPT-2 table.
fn is_direct(byte: u8) -> bool {
    matches!(byte, 0x21..=0x7e | 0xa1..=0xac | 0xae..=0xff)
}

/// The character a byte level tokenizer uses for one byte.
pub fn byte_to_character(byte: u8) -> char {
    if is_direct(byte) {
        return char::from(byte);
    }
    let mut offset = 0u32;
    for candidate in 0..byte {
        if !is_direct(candidate) {
            offset += 1;
        }
    }
    char::from_u32(0x100 + offset).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// The one character token string of one byte.
pub fn token_for_byte(byte: u8) -> String {
    let mut buffer = [0u8; 4];
    byte_to_character(byte).encode_utf8(&mut buffer).to_string()
}

/// The byte a character of the GPT-2 table stands for.
fn unicode_to_byte(character: char) -> Option<u8> {
    let code = character as u32;
    if let Ok(byte) = u8::try_from(code) {
        if is_direct(byte) {
            return Some(byte);
        }
    }
    if !(0x100..=0x143).contains(&code) {
        return None;
    }
    let target = code - 0x100;
    let mut offset = 0u32;
    for byte in 0..=255u8 {
        if !is_direct(byte) {
            if offset == target {
                return Some(byte);
            }
            offset += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{byte_to_character, token_for_byte, Vocab};

    #[test]
    fn every_byte_round_trips_through_the_table() {
        let mut ids = Vec::new();
        let mut json = String::from("{");
        for byte in 0..=255u8 {
            let token = token_for_byte(byte);
            ids.push(u32::from(byte));
            if byte != 0 {
                json.push(',');
            }
            json.push_str(&format!("\"{}\":{byte}", escape(&token)));
        }
        json.push('}');
        let vocab = Vocab::parse(json.as_bytes()).unwrap();
        let decoded = vocab.decode(&ids).unwrap();
        assert_eq!(decoded.bytes, (0..=255u8).collect::<Vec<u8>>());
        assert_eq!(decoded.outside_alphabet, 0);
    }

    #[test]
    fn parses_a_small_file_and_reports_unknown_ids() {
        let vocab = Vocab::parse(br#"{"\u0120":0,"a":2}"#).unwrap();
        assert_eq!(vocab.token(0), Some("\u{0120}"));
        assert_eq!(vocab.token(1), None);
        assert_eq!(vocab.token(2), Some("a"));
        assert_eq!(vocab.token(3), None);
        assert_eq!(vocab.decode(&[0, 2]).unwrap().bytes, vec![b' ', b'a']);
        assert!(vocab.decode(&[7]).is_err());
    }

    #[test]
    fn counts_characters_outside_the_byte_alphabet() {
        let vocab = Vocab::parse("{\"\u{0144}\":0}".as_bytes()).unwrap();
        let decoded = vocab.decode(&[0]).unwrap();
        assert_eq!(decoded.outside_alphabet, 1);
        assert_eq!(decoded.bytes, "\u{0144}".as_bytes());
    }

    #[test]
    fn covers_requires_every_id_in_range() {
        let vocab = Vocab::parse(br#"{"a":0,"b":2}"#).unwrap();
        assert!(!vocab.covers(3));
        let vocab = Vocab::parse(br#"{"a":0,"b":1}"#).unwrap();
        assert!(vocab.covers(2));
        assert!(!vocab.covers(3));
    }

    #[test]
    fn rejects_a_duplicate_id() {
        assert!(Vocab::parse(br#"{"a":0,"b":0}"#).is_err());
    }

    #[test]
    fn direct_bytes_keep_their_character() {
        assert_eq!(byte_to_character(b'!'), '!');
        assert_eq!(byte_to_character(b' '), '\u{0120}');
    }

    fn escape(token: &str) -> String {
        let mut out = String::new();
        for character in token.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                _ => out.push(character),
            }
        }
        out
    }
}
