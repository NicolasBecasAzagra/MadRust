// SPDX-License-Identifier: GPL-2.0-only
//! The prompt encoder.
//!
//! The agent builds its conversation at run time, so it has to produce the
//! token ids the checkpoint was trained to read. When the model root holds a
//! `merges.txt`, the encoder is the byte pair merge encoder of
//! [`madrust_infer::bpe`]: the GPT-2 pre-tokenizer, the byte level alphabet and
//! the merges of the checkpoint, which is the same path the reference
//! tokenizer takes. A model without the file, such as the synthetic
//! development model of `madrust-infer-gen`, falls back to
//! [`madrust_infer::prompt::ByteTokens`], one token per byte: the vocabulary of
//! the synthetic model has a token for every byte, so the encoding is still
//! exact, with more tokens to move the same bytes. The vocabulary of a real
//! checkpoint has a token for only 235 of the 256 bytes, so the fallback is a
//! development path and not a substitute for the merges.
//!
//! The chat markers are special when the vocabulary carries them.
//! `<|im_start|>` and `<|im_end|>` are single special tokens in the real
//! checkpoint, so the encoder splits the prompt on them, encodes the plain
//! parts with the text encoder, and inserts the marker id from
//! [`Vocab::find_token`](madrust_infer::vocab::Vocab::find_token). A marker the
//! vocabulary does not carry is not fatal: the synthetic development model has
//! a byte level vocabulary without the markers, so the encoder falls back to
//! the marker's plain bytes and says so on one log line. The real run pays one
//! token per marker; the development run pays a few bytes and the log explains
//! why.

use madrust_infer::bpe::Bpe;
use madrust_infer::chat;
use madrust_infer::error::Result;
use madrust_infer::paths;
use madrust_infer::prompt::ByteTokens;
use madrust_infer::vocab::Vocab;

/// The encoder for one vocabulary.
pub struct Encoder {
    text: Text,
    start_marker: Option<u32>,
    end_marker: Option<u32>,
}

/// How plain text becomes token ids.
enum Text {
    /// The merges of the checkpoint, the path a real model takes.
    Merge(Bpe),
    /// One token per byte, the path of a model without a merges file. The
    /// table is boxed because the 256 token ids would otherwise be the size of
    /// the whole enum, and the merge table is the small variant.
    Byte(Box<ByteTokens>),
}

impl Encoder {
    /// Build the encoder from the model root.
    ///
    /// A readable `merges.txt` selects the merge encoder. Without the file the
    /// encoder is the byte per byte one, which refuses a vocabulary that has no
    /// token for every byte. The mode appears on one log line, so a run says
    /// which path it took.
    pub fn new(vocab: &Vocab) -> Result<Self> {
        let (root, _) = paths::resolve()?;
        let merges = std::fs::read(&root.merges).ok();
        Self::from_merges(vocab, merges.as_deref())
    }

    /// The pure half of [`new`](Self::new), so both modes are tested without a
    /// file system.
    fn from_merges(vocab: &Vocab, merges: Option<&[u8]>) -> Result<Self> {
        let missing = chat::missing_markers(vocab);
        if !missing.is_empty() {
            println!(
                "AGENT encoder markers={} as=bytes reason=vocabulary has no token",
                missing.join(", ")
            );
        }
        let text = match merges {
            Some(bytes) => {
                println!("AGENT encoder mode=merge source=merges.txt");
                Text::Merge(Bpe::new(bytes)?)
            }
            None => {
                println!("AGENT encoder mode=bytes reason=no merges.txt");
                Text::Byte(Box::new(ByteTokens::new(vocab)?))
            }
        };
        Ok(Self {
            text,
            start_marker: vocab.find_token(chat::IM_START),
            end_marker: vocab.find_token(chat::IM_END),
        })
    }

    /// Encode one prompt: plain text plus the marker token ids.
    ///
    /// The vocabulary is passed in rather than kept by the encoder: the agent
    /// holds it inside the runtime, which borrows it mutably again to generate
    /// right after the prompt is encoded.
    pub fn encode(&self, vocab: &Vocab, text: &str) -> Result<Vec<u32>> {
        let mut ids = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let next = earliest_marker(rest);
            match next {
                Some((index, marker)) => {
                    if index != 0 {
                        ids.extend(self.plain(vocab, &rest[..index])?);
                    }
                    match self.marker_id(marker) {
                        Some(id) => ids.push(id),
                        None => ids.extend(self.plain(vocab, marker)?),
                    }
                    rest = &rest[index + marker.len()..];
                }
                None => {
                    ids.extend(self.plain(vocab, rest)?);
                    break;
                }
            }
        }
        Ok(ids)
    }

    /// Encode plain text with the mode of this encoder.
    fn plain(&self, vocab: &Vocab, text: &str) -> Result<Vec<u32>> {
        match &self.text {
            Text::Merge(bpe) => bpe.encode(vocab, text),
            Text::Byte(bytes) => bytes.encode(text.as_bytes()),
        }
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
    /// flags ask for at ids 256 and 257, plus the merged tokens 258 and 259.
    fn synthetic_vocab(include_start: bool, include_end: bool) -> Vocab {
        let mut tokens: Vec<String> = (0..=255u8).map(token_for_byte).collect();
        tokens.sort();
        if include_start {
            tokens.push(IM_START.to_string());
        }
        if include_end {
            tokens.push(IM_END.to_string());
        }
        tokens.push("ab".to_string());
        tokens.push("\u{0120}c".to_string());
        let mut json = String::from("{");
        for (id, token) in tokens.iter().enumerate() {
            if id != 0 {
                json.push(',');
            }
            push_json_string(&mut json, token);
            json.push(':');
            json.push_str(&id.to_string());
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

    /// The merges that turn `a b` into `ab` and `Ġ c` into `Ġc`.
    const MERGES: &[u8] = b"#version: 0.2\na b\n\xc4\xa0 c\n";

    #[test]
    fn without_merges_the_encoder_is_one_token_per_byte() {
        let vocab = synthetic_vocab(true, true);
        let encoder = Encoder::from_merges(&vocab, None).expect("the markers exist");
        let ids = encoder
            .encode(&vocab, "ab c")
            .expect("every byte has a token");
        assert_eq!(
            ids,
            vec![byte_id(b'a'), byte_id(b'b'), byte_id(b' '), byte_id(b'c')]
        );
    }

    #[test]
    fn the_constructor_reads_the_model_root() {
        let vocab = synthetic_vocab(true, true);
        // The guest root of a host run has no merges file, so the constructor
        // falls back; when MADRUST_INFER_ROOT names a checkpoint it selects the
        // merge encoder instead. Both are a valid encoder for this vocabulary.
        assert!(Encoder::new(&vocab).is_ok());
    }

    #[test]
    fn with_merges_the_encoder_uses_the_merge_table() {
        let vocab = synthetic_vocab(true, true);
        let encoder = Encoder::from_merges(&vocab, Some(MERGES)).expect("the merges parse");
        let ids = encoder
            .encode(&vocab, "ab c")
            .expect("every piece has a token");
        assert_eq!(ids, [258, 259]);
    }

    #[test]
    fn puts_the_marker_ids_where_the_text_has_the_markers() {
        let vocab = synthetic_vocab(true, true);
        let encoder = Encoder::from_merges(&vocab, None).expect("the markers exist");
        let ids = encoder
            .encode(&vocab, &format!("a{IM_START}b{IM_END}c"))
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
        let encoder = Encoder::from_merges(&vocab, None).expect("the byte table is complete");
        let ids = encoder
            .encode(&vocab, &format!("a{IM_END}b"))
            .expect("the prompt encodes");

        let mut expected = vec![byte_id(b'a')];
        expected.extend(IM_END.bytes().map(byte_id));
        expected.push(byte_id(b'b'));
        assert_eq!(ids, expected);
        assert!(!ids.contains(&256));
    }

    #[test]
    fn the_merge_encoder_keeps_the_marker_ids_between_merged_pieces() {
        let vocab = synthetic_vocab(true, true);
        let encoder = Encoder::from_merges(&vocab, Some(MERGES)).expect("the merges parse");
        let ids = encoder
            .encode(&vocab, &format!("ab{IM_START} c"))
            .expect("the prompt encodes");
        assert_eq!(ids, [258, 256, 259]);
    }

    #[test]
    fn both_missing_markers_become_their_bytes() {
        let vocab = synthetic_vocab(false, false);
        assert_eq!(
            madrust_infer::chat::missing_markers(&vocab),
            vec![IM_START, IM_END]
        );
        let encoder = Encoder::from_merges(&vocab, None).expect("the byte table is complete");
        let ids = encoder
            .encode(&vocab, &format!("x{IM_START}{IM_END}"))
            .expect("the prompt encodes");
        assert_eq!(ids.len(), 1 + IM_START.len() + IM_END.len());
        assert!(ids.iter().all(|&id| id < 256));
    }
}
