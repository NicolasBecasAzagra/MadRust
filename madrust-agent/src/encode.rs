// SPDX-License-Identifier: GPL-2.0-only
//! The prompt encoder.
//!
//! The runtime has no pre-tokenizer: [`madrust_infer::prompt::ByteTokens`]
//! turns every byte of the plain text into one token. That is exact, because
//! the byte level vocabulary carries a token for all 256 bytes.
//!
//! The chat markers are special when the vocabulary carries them.
//! `<|im_start|>` and `<|im_end|>` are single special tokens in the real
//! checkpoint, so the encoder splits the prompt on them, encodes the plain
//! parts one byte per token, and inserts the marker id from
//! [`Vocab::find_token`](madrust_infer::vocab::Vocab::find_token). A marker the
//! vocabulary does not carry is not fatal: the synthetic development model has
//! a byte level vocabulary without the markers, so the encoder falls back to
//! the marker's plain bytes and says so on one log line. The real run pays one
//! token per marker; the development run pays a few bytes and the log explains
//! why.

use madrust_infer::chat;
use madrust_infer::error::Result;
use madrust_infer::prompt::ByteTokens;
use madrust_infer::vocab::Vocab;

/// The encoder for one vocabulary.
pub struct Encoder {
    bytes: ByteTokens,
    start_marker: Option<u32>,
    end_marker: Option<u32>,
}

impl Encoder {
    /// Build the byte table and resolve the two chat markers.
    ///
    /// A marker that is one exact token in the vocabulary is inserted as that
    /// token. A marker the vocabulary lacks is written as its plain bytes, and
    /// the encoder prints one line naming the markers that took that path.
    pub fn new(vocab: &Vocab) -> Result<Self> {
        let missing = chat::missing_markers(vocab);
        if !missing.is_empty() {
            println!(
                "AGENT encoder markers={} as=bytes reason=vocabulary has no token",
                missing.join(", ")
            );
        }
        Ok(Self {
            bytes: ByteTokens::new(vocab)?,
            start_marker: vocab.find_token(chat::IM_START),
            end_marker: vocab.find_token(chat::IM_END),
        })
    }

    /// Encode one prompt: plain bytes plus the marker token ids.
    pub fn encode(&self, text: &str) -> Result<Vec<u32>> {
        let mut ids = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let next = earliest_marker(rest);
            match next {
                Some((index, marker)) => {
                    if index != 0 {
                        ids.extend(self.bytes.encode(&rest.as_bytes()[..index])?);
                    }
                    match self.marker_id(marker) {
                        Some(id) => ids.push(id),
                        None => ids.extend(self.bytes.encode(marker.as_bytes())?),
                    }
                    rest = &rest[index + marker.len()..];
                }
                None => {
                    ids.extend(self.bytes.encode(rest.as_bytes())?);
                    break;
                }
            }
        }
        Ok(ids)
    }

    /// The token id of one marker, when the vocabulary carries it.
    fn marker_id(&self, marker: &str) -> Option<u32> {
        if marker == chat::IM_START {
            self.start_marker
        } else {
            self.end_marker
        }
    }
}

/// The first marker in `text` and the text it matches.
fn earliest_marker(text: &str) -> Option<(usize, &'static str)> {
    match (text.find(chat::IM_START), text.find(chat::IM_END)) {
        (Some(start), Some(end)) if start <= end => Some((start, chat::IM_START)),
        (Some(_), Some(end)) => Some((end, chat::IM_END)),
        (Some(start), None) => Some((start, chat::IM_START)),
        (None, Some(end)) => Some((end, chat::IM_END)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Encoder;
    use madrust_infer::chat::{IM_END, IM_START};
    use madrust_infer::vocab::{token_for_byte, Vocab};

    /// A byte level vocabulary with ids 0 to 255, plus the chat markers the
    /// flags ask for at fixed ids 256 and 257.
    fn synthetic_vocab(include_start: bool, include_end: bool) -> Vocab {
        let mut tokens: Vec<String> = (0..=255u8).map(token_for_byte).collect();
        tokens.sort();
        let mut json = String::from("{");
        for (id, token) in tokens.iter().enumerate() {
            if id != 0 {
                json.push(',');
            }
            push_json_string(&mut json, token);
            json.push(':');
            json.push_str(&id.to_string());
        }
        if include_start {
            json.push(',');
            push_json_string(&mut json, IM_START);
            json.push_str(":256");
        }
        if include_end {
            json.push(',');
            push_json_string(&mut json, IM_END);
            json.push_str(":257");
        }
        json.push('}');
        Vocab::parse(json.as_bytes()).expect("the synthetic vocabulary is valid")
    }

    fn push_json_string(out: &mut String, value: &str) {
        out.push('"');
        for character in value.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                _ => out.push(character),
            }
        }
        out.push('"');
    }

    fn byte_id(byte: u8) -> u32 {
        let mut tokens: Vec<String> = (0..=255u8).map(token_for_byte).collect();
        tokens.sort();
        tokens
            .iter()
            .position(|token| token == &token_for_byte(byte))
            .expect("the byte has a token") as u32
    }

    #[test]
    fn encodes_plain_text_one_byte_per_token() {
        let vocab = synthetic_vocab(true, true);
        let encoder = Encoder::new(&vocab).expect("the markers exist");
        let ids = encoder.encode("abc").expect("every byte has a token");
        assert_eq!(ids, vec![byte_id(b'a'), byte_id(b'b'), byte_id(b'c')]);
    }

    #[test]
    fn puts_the_marker_ids_where_the_text_has_the_markers() {
        let vocab = synthetic_vocab(true, true);
        let encoder = Encoder::new(&vocab).expect("the markers exist");
        let ids = encoder
            .encode(&format!("a{IM_START}b{IM_END}c"))
            .expect("the prompt encodes");
        assert_eq!(ids.len(), 5);
        assert_eq!(ids[0], byte_id(b'a'));
        assert_eq!(ids[1], 256);
        assert_eq!(ids[2], byte_id(b'b'));
        assert_eq!(ids[3], 257);
        assert_eq!(ids[4], byte_id(b'c'));
    }

    #[test]
    fn a_missing_marker_is_encoded_as_its_bytes() {
        let vocab = synthetic_vocab(true, false);
        let encoder = Encoder::new(&vocab).expect("the byte table is complete");
        let ids = encoder
            .encode(&format!("a{IM_END}b"))
            .expect("the prompt encodes");

        let mut expected = vec![byte_id(b'a')];
        expected.extend(IM_END.bytes().map(byte_id));
        expected.push(byte_id(b'b'));
        assert_eq!(ids, expected);
        assert!(!ids.contains(&257));
    }

    #[test]
    fn both_missing_markers_become_their_bytes() {
        let vocab = synthetic_vocab(false, false);
        assert_eq!(
            madrust_infer::chat::missing_markers(&vocab),
            vec![IM_START, IM_END]
        );
        let encoder = Encoder::new(&vocab).expect("the byte table is complete");
        let ids = encoder
            .encode(&format!("x{IM_START}{IM_END}"))
            .expect("the prompt encodes");
        assert_eq!(ids.len(), 1 + IM_START.len() + IM_END.len());
        assert!(ids.iter().all(|&id| id < 256));
    }
}
