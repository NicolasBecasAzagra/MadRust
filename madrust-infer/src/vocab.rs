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

    /// Number of ids the map covers, including a gap for a missing id.
    ///
    /// A complete vocabulary has this equal to the model vocabulary size; an
    /// id at or above it has no token string.
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// The id of an exact token string, if the map holds it.
    ///
    /// The vocabulary is stored in the other direction, so this is a linear
    /// scan. It exists for the chat markers, which are a few fixed strings and
    /// are checked once per run.
    pub fn find_token(&self, token: &str) -> Option<u32> {
        for (id, slot) in self.tokens.iter().enumerate() {
            let Some((start, end)) = *slot else {
                continue;
            };
            if self.text.get(start..end) == Some(token) {
                return u32::try_from(id).ok();
            }
        }
        None
    }

    /// A fingerprint of the vocabulary.
    ///
    /// A cache that holds a mask of tokens must not hand it to another
    /// vocabulary. The fingerprint covers the token count and the bytes of the
    /// token strings, so two vocabularies with the same count but different
    /// tokens get different values and the cache is rebuilt.
    pub fn fingerprint(&self) -> u64 {
        let count = self.tokens.len().to_le_bytes();
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in self.text.as_bytes().iter().chain(count.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
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
                match character_to_byte(character) {
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

    /// The original bytes of one token id.
    ///
    /// The GPT-2 style byte level alphabet writes one character per byte, so
    /// every character of the token string maps back to its byte. `None` means
    /// the id has no token string or the string has a character outside that
    /// alphabet (a special token, for example), which a byte grammar cannot
    /// consume.
    pub fn token_bytes(&self, id: u32) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        self.token_bytes_into(id, &mut bytes).then_some(bytes)
    }

    /// Write the original bytes of one token id into `out`.
    ///
    /// `out` is cleared first. Returns `false` when the id has no token string
    /// or a character outside the byte level alphabet. The buffer form exists
    /// for [`crate::grammar::Grammar::allowed_tokens`], which tests every token
    /// of a vocabulary against one state.
    pub fn token_bytes_into(&self, id: u32, out: &mut Vec<u8>) -> bool {
        out.clear();
        let Some(token) = self.token(id) else {
            return false;
        };
        for character in token.chars() {
            match character_to_byte(character) {
                Some(byte) => out.push(byte),
                None => {
                    out.clear();
                    return false;
                }
            }
        }
        true
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
///
/// This is the inverse of [`byte_to_character`]: every byte has exactly one
/// character and every character produced by the table maps back to that byte.
/// `None` means the character is not part of the byte level alphabet.
pub fn character_to_byte(character: char) -> Option<u8> {
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
    use super::{byte_to_character, character_to_byte, token_for_byte, Vocab};
    use std::path::{Path, PathBuf};

    #[test]
    fn the_inverse_table_round_trips_every_byte() {
        for byte in 0..=255u8 {
            assert_eq!(character_to_byte(byte_to_character(byte)), Some(byte));
        }
        assert_eq!(character_to_byte('\u{0144}'), None);
    }

    #[test]
    fn the_inverse_map_reproduces_a_host_vocabulary() {
        let Some(path) = host_vocab() else {
            return;
        };
        let bytes = std::fs::read(&path).expect("the host vocabulary is readable");
        let vocab = Vocab::parse(&bytes).expect("the host vocabulary parses");
        let mut raw = Vec::new();
        let mut checked = 0usize;
        for index in 0..vocab.token_count() {
            let Ok(id) = u32::try_from(index) else {
                break;
            };
            let Some(token) = vocab.token(id) else {
                continue;
            };
            if !vocab.token_bytes_into(id, &mut raw) {
                continue;
            }
            let rebuilt: String = raw.iter().map(|&byte| byte_to_character(byte)).collect();
            assert_eq!(rebuilt, token, "token {id} does not round trip");
            checked += 1;
        }
        assert!(checked > 1000, "only {checked} tokens round tripped");
    }

    /// The vocabulary the native comparison uses, when this host has it.
    fn host_vocab() -> Option<PathBuf> {
        if let Ok(root) = std::env::var("MADRUST_INFER_ROOT") {
            let path = Path::new(&root).join("vocab.json");
            if path.is_file() {
                return Some(path);
            }
        }
        let home = std::env::var_os("HOME")?;
        let path = Path::new(&home)
            .join("madrust")
            .join("artifacts")
            .join("model")
            .join("vocab.json");
        path.is_file().then_some(path)
    }

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
