// SPDX-License-Identifier: GPL-2.0-only
//! Encoding a prompt that the caller built.
//!
//! The runtime has no pre-tokenizer, so a prompt that is built at run time
//! (the conversation of an agent) is encoded one byte per token. A byte level
//! vocabulary holds a token for every one of the 256 bytes, so the encoding is
//! exact: the model reads the same bytes a merged encoding would carry, with
//! more tokens to move them. The price is a longer prefill, which the log of
//! the agent run reports.
//!
//! The table is built once per vocabulary, by reading the tokens whose string
//! is a single character of the byte level alphabet.

use crate::error::{Error, Result};
use crate::vocab::{character_to_byte, Vocab};

/// The token id of every byte of the alphabet.
pub struct ByteTokens {
    ids: [Option<u32>; 256],
}

impl ByteTokens {
    /// Build the table from a vocabulary.
    ///
    /// A vocabulary that does not hold every byte is refused: a prompt that
    /// cannot be encoded byte for byte would reach the model as something
    /// else, and the agent would talk about a conversation the model never
    /// saw.
    pub fn new(vocab: &Vocab) -> Result<Self> {
        let mut ids: [Option<u32>; 256] = [None; 256];
        for id in 0..vocab.token_count() {
            let id = u32::try_from(id)
                .map_err(|_| Error::new("prompt: the vocabulary has too many tokens"))?;
            let Some(token) = vocab.token(id) else {
                continue;
            };
            let mut characters = token.chars();
            let Some(character) = characters.next() else {
                continue;
            };
            if characters.next().is_some() {
                continue;
            }
            let Some(byte) = character_to_byte(character) else {
                continue;
            };
            ids[byte as usize] = Some(id);
        }
        if ids.iter().any(|slot| slot.is_none()) {
            return Err(Error::new(
                "prompt: the vocabulary has no token for every byte of the alphabet",
            ));
        }
        Ok(Self { ids })
    }

    /// The token id of one byte.
    pub fn get(&self, byte: u8) -> Option<u32> {
        self.ids[byte as usize]
    }

    /// Encode bytes as token ids, one token per byte.
    pub fn encode(&self, text: &[u8]) -> Result<Vec<u32>> {
        let mut ids = Vec::with_capacity(text.len());
        for byte in text {
            let id = self.get(*byte).ok_or_else(|| {
                Error::new(format!(
                    "prompt: the vocabulary has no token for byte {byte}"
                ))
            })?;
            ids.push(id);
        }
        Ok(ids)
    }

    /// Number of bytes the table covers.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// `true` when the table covers no byte at all.
    pub fn is_empty(&self) -> bool {
        self.ids.iter().all(|slot| slot.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocab::token_for_byte;

    /// A vocabulary with one token per byte of the alphabet.
    fn synthetic_vocab(bytes: &[u8]) -> Vocab {
        let mut tokens: Vec<String> = bytes.iter().copied().map(token_for_byte).collect();
        tokens.sort();
        tokens.dedup();
        let mut json = String::from("{");
        for (id, token) in tokens.iter().enumerate() {
            if id != 0 {
                json.push(',');
            }
            json.push('"');
            for character in token.chars() {
                match character {
                    '"' => json.push_str("\\\""),
                    '\\' => json.push_str("\\\\"),
                    _ => json.push(character),
                }
            }
            json.push_str("\":");
            json.push_str(&id.to_string());
        }
        json.push('}');
        Vocab::parse(json.as_bytes()).expect("the synthetic vocabulary is valid")
    }

    #[test]
    fn every_byte_has_its_token() {
        let bytes: Vec<u8> = (0..=255).collect();
        let vocab = synthetic_vocab(&bytes);
        let table = ByteTokens::new(&vocab).expect("the table is complete");
        assert_eq!(table.len(), 256);
        assert!(!table.is_empty());
        for byte in 0..=255u8 {
            assert!(table.get(byte).is_some(), "byte {byte} has no token");
        }
    }

    #[test]
    fn the_encoding_is_one_token_per_byte() {
        let bytes: Vec<u8> = (0..=255).collect();
        let vocab = synthetic_vocab(&bytes);
        let table = ByteTokens::new(&vocab).expect("the table is complete");
        let text = b"{\"tool\":\"read\"}";
        let ids = table.encode(text).expect("every byte has a token");
        assert_eq!(ids.len(), text.len());
        for (id, byte) in ids.iter().zip(text) {
            assert_eq!(table.get(*byte), Some(*id));
        }
    }

    #[test]
    fn a_vocabulary_without_every_byte_is_refused() {
        let vocab = synthetic_vocab(b"ab");
        assert!(ByteTokens::new(&vocab).is_err());
    }
}
