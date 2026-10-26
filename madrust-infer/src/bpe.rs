// SPDX-License-Identifier: GPL-2.0-only
//! The byte pair merge encoder.
//!
//! A checkpoint is trained on the pieces of the GPT-2 pre-tokenizer and on the
//! merges of its own `merges.txt`. A prompt that the caller builds at run time
//! reaches the model as text, so the runtime needs both halves to produce the
//! ids the reference tokenizer would produce.
//!
//! The pre-tokenizer is the GPT-2 pattern
//!
//! ```text
//! 's|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+
//! ```
//!
//! written by hand because this crate has no dependencies. The alternatives are
//! tried in that order at every position, the character classes come from the
//! standard library (`char::is_alphabetic` is `\p{L}`, `char::is_numeric` is
//! `\p{N}`), and `\s+(?!\S)` is a whitespace run whose look-ahead must not see a
//! non-space: a run that a word follows gives up its last space, which is what
//! makes the space join the next piece.
//!
//! Every piece is then written in the byte level alphabet of the runtime
//! ([`crate::vocab::byte_to_character`]), a byte that is printable stays
//! itself, and every other byte becomes one character outside ASCII. The
//! checkpoint writes its merges in that alphabet, so a pair is compared as the
//! bytes of the two symbols.
//!
//! ## Storage of the merges
//!
//! A checkpoint has tens of thousands of merges and the guest gives the
//! program a small heap, so the table is not one `String` per merge. It is one
//! flat buffer with an index: the records are stored back to back in rank
//! order, the index holds the offset of every record plus the length of its
//! first side, and a second index lists the ranks sorted by pair, which is what
//! the binary search reads. A pair of a few bytes therefore costs its bytes
//! plus ten bytes of index, and the whole table costs four allocations.
//!
//! For `SmolLM2-135M`, whose file has 48,900 merges, the records take 417,477
//! bytes, the offsets 195,604, the first side lengths 97,800 and the sorted
//! ranks 195,600: 906,481 bytes, about 885 KiB, in four allocations.

use core::cmp::Ordering;

use crate::error::{Error, Result};
use crate::vocab::{byte_to_character, Vocab};

/// The contractions of the pattern, in the order it tries them.
const CONTRACTIONS: [&str; 7] = ["'s", "'t", "'re", "'ve", "'m", "'ll", "'d"];

/// The merges of a checkpoint.
pub struct Bpe {
    /// Every pair record, `first second`, in rank order, without the newline.
    records: Vec<u8>,
    /// Offset of every record in `records`, plus one past the last record.
    starts: Vec<u32>,
    /// Length of the first side of every record.
    first: Vec<u16>,
    /// Every rank, ordered by the bytes of its record.
    sorted: Vec<u32>,
}

impl Bpe {
    /// Parse the merges of a checkpoint.
    ///
    /// The file is the GPT-2 format: an optional `#version: 0.2` line, then one
    /// pair per line, two strings separated by one space. The rank of a pair is
    /// its line in the file, counted from zero after the header. The bytes are
    /// not interpreted as text: a pair is compared as bytes, so a malformed
    /// UTF-8 line cannot make the parser fail.
    pub fn new(merges: &[u8]) -> Result<Self> {
        let mut records = Vec::with_capacity(merges.len());
        let mut starts: Vec<u32> = Vec::with_capacity(merges.len() / 8 + 2);
        let mut first: Vec<u16> = Vec::with_capacity(merges.len() / 8 + 1);
        starts.push(0);

        for (index, line) in merges.split(|byte| *byte == b'\n').enumerate() {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            if index == 0 && line.starts_with(b"#") {
                continue;
            }
            let Some(space) = line.iter().position(|byte| *byte == b' ') else {
                return Err(Error::new(format!(
                    "bpe: merges line {} has no separator",
                    index + 1
                )));
            };
            if space == 0 || space + 1 == line.len() || line[space + 1..].contains(&b' ') {
                return Err(Error::new(format!(
                    "bpe: merges line {} is not two tokens",
                    index + 1
                )));
            }
            let Ok(first_length) = u16::try_from(space) else {
                return Err(Error::new(format!(
                    "bpe: merges line {} has a token longer than 65535 bytes",
                    index + 1
                )));
            };
            records.extend_from_slice(line);
            let Ok(end) = u32::try_from(records.len()) else {
                return Err(Error::new("bpe: the merges file is larger than 4 GiB"));
            };
            starts.push(end);
            first.push(first_length);
        }

        let count = u32::try_from(first.len())
            .map_err(|_| Error::new("bpe: the merges file has more than 2^32 pairs"))?;
        let mut sorted: Vec<u32> = (0..count).collect();
        sorted.sort_unstable_by(|left, right| {
            record(&records, &starts, *left).cmp(record(&records, &starts, *right))
        });
        for pair in sorted.windows(2) {
            let left = record(&records, &starts, pair[0]);
            let right = record(&records, &starts, pair[1]);
            if left == right {
                return Err(Error::new(format!(
                    "bpe: duplicate merge {}",
                    String::from_utf8_lossy(left)
                )));
            }
        }

        Ok(Self {
            records,
            starts,
            first,
            sorted,
        })
    }

    /// Encode text into the token ids of `vocab`.
    ///
    /// The text is split by the pre-tokenizer, every piece is written in the
    /// byte level alphabet, the merge loop joins symbols while a pair has a
    /// rank, and every symbol that is left is looked up in the vocabulary. A
    /// vocabulary without the token of a final symbol is an error: the model
    /// would read a conversation the caller never wrote.
    pub fn encode(&self, vocab: &Vocab, text: &str) -> Result<Vec<u32>> {
        let alphabet: [char; 256] = core::array::from_fn(|byte| byte_to_character(byte as u8));
        let mut ids = Vec::new();
        let mut piece = String::new();
        let mut symbols = Vec::new();

        for (start, end) in pieces(text) {
            piece.clear();
            for byte in &text.as_bytes()[start..end] {
                piece.push(alphabet[*byte as usize]);
            }
            self.merge(&piece, &mut symbols);
            ids.reserve(symbols.len());
            for &(symbol_start, symbol_end) in &symbols {
                let token = &piece[symbol_start..symbol_end];
                let id = vocab.find_token(token).ok_or_else(|| {
                    Error::new(format!("bpe: the vocabulary has no token for {token:?}"))
                })?;
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// The rank of a pair, when the table holds it.
    fn rank(&self, first: &[u8], second: &[u8]) -> Option<u32> {
        let mut low = 0usize;
        let mut high = self.sorted.len();
        while low < high {
            let middle = low + (high - low) / 2;
            let rank = self.sorted[middle];
            let ordering = match self.sides(rank) {
                Some((stored_first, stored_second)) => stored_first
                    .cmp(first)
                    .then_with(|| stored_second.cmp(second)),
                None => Ordering::Less,
            };
            match ordering {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => return Some(rank),
            }
        }
        None
    }

    /// Join the adjacent pair with the lowest rank until no pair has one.
    ///
    /// `symbols` is refilled with the byte ranges of the symbols that are left,
    /// so a merged symbol is always one range of `piece` and the concatenation
    /// is never copied.
    fn merge(&self, piece: &str, symbols: &mut Vec<(usize, usize)>) {
        symbols.clear();
        symbols.extend(
            piece
                .char_indices()
                .map(|(at, character)| (at, at + character.len_utf8())),
        );
        let bytes = piece.as_bytes();
        while symbols.len() > 1 {
            let mut best: Option<(u32, usize)> = None;
            for (index, pair) in symbols.windows(2).enumerate() {
                let first = &bytes[pair[0].0..pair[0].1];
                let second = &bytes[pair[1].0..pair[1].1];
                let Some(rank) = self.rank(first, second) else {
                    continue;
                };
                let lower = match best {
                    Some((lowest, _)) => rank < lowest,
                    None => true,
                };
                if lower {
                    best = Some((rank, index));
                }
            }
            let Some((_, index)) = best else {
                break;
            };
            symbols[index].1 = symbols[index + 1].1;
            symbols.remove(index + 1);
        }
    }

    /// The record of one rank.
    fn record(&self, rank: u32) -> &[u8] {
        record(&self.records, &self.starts, rank)
    }

    /// The two sides of the record of one rank, when the index is consistent.
    fn sides(&self, rank: u32) -> Option<(&[u8], &[u8])> {
        let record = self.record(rank);
        let length = usize::from(*self.first.get(rank as usize)?);
        if record.get(length) != Some(&b' ') {
            return None;
        }
        Some((record.get(..length)?, record.get(length + 1..)?))
    }
}

/// One record of the flat buffer, `first second`, without the newline.
fn record<'a>(records: &'a [u8], starts: &[u32], rank: u32) -> &'a [u8] {
    let start = starts.get(rank as usize).copied().unwrap_or(0) as usize;
    let end = starts.get(rank as usize + 1).copied().unwrap_or(0) as usize;
    records.get(start..end).unwrap_or(&[])
}

/// The GPT-2 pre-tokenizer: the byte ranges of the pieces of `text`.
fn pieces(text: &str) -> Vec<(usize, usize)> {
    let mut pieces = Vec::new();
    let mut at = 0usize;
    while at < text.len() {
        let end = next_piece(text, at);
        pieces.push((at, end));
        at = end;
    }
    pieces
}

/// The end of the piece that starts at `at`.
///
/// The alternatives run in the order of the pattern. The optional space is
/// ASCII: a tab or a non-breaking space is whitespace, not the space the
/// pattern can keep.
fn next_piece(text: &str, at: usize) -> usize {
    let rest = &text[at..];
    for contraction in CONTRACTIONS {
        if rest.starts_with(contraction) {
            return at + contraction.len();
        }
    }
    let after_space = if char_at(text, at) == Some(' ') {
        at + 1
    } else {
        at
    };
    for member in [is_letter, is_number, is_other] {
        let end = run_end(text, after_space, member);
        if end > after_space {
            return end;
        }
    }

    // `\s+(?!\S)`: a greedy run that gives its last space back when a word
    // follows, because the look-ahead must not see a non-space there.
    let mut end = at;
    let mut last = at;
    while let Some(character) = char_at(text, end) {
        if !character.is_whitespace() {
            break;
        }
        last = end;
        end += character.len_utf8();
    }
    if end == text.len() {
        return end;
    }
    if last > at {
        return last;
    }
    if end > at {
        // `\s+` with one whitespace character before a word.
        return end;
    }

    // Every character is a letter, a number, whitespace or something else, so
    // one of the alternatives always matches. Keep the fallback total anyway.
    match char_at(text, at) {
        Some(character) => at + character.len_utf8(),
        None => text.len(),
    }
}

/// The character at one byte offset of `text`, which must be a boundary.
fn char_at(text: &str, at: usize) -> Option<char> {
    text.get(at..)?.chars().next()
}

/// The end of the run of characters of `member` that starts at `at`.
fn run_end(text: &str, at: usize, member: fn(char) -> bool) -> usize {
    let mut end = at;
    while let Some(character) = char_at(text, end) {
        if !member(character) {
            break;
        }
        end += character.len_utf8();
    }
    end
}

/// `\p{L}`.
fn is_letter(character: char) -> bool {
    character.is_alphabetic()
}

/// `\p{N}`.
fn is_number(character: char) -> bool {
    character.is_numeric()
}

/// `[^\s\p{L}\p{N}]`.
fn is_other(character: char) -> bool {
    !character.is_whitespace() && !character.is_alphabetic() && !character.is_numeric()
}

#[cfg(test)]
mod tests {
    use super::{pieces, Bpe};
    use crate::vocab::{token_for_byte, Vocab};
    use std::path::{Path, PathBuf};

    /// The pieces of `text`, as strings.
    fn split(text: &str) -> Vec<&str> {
        pieces(text)
            .into_iter()
            .map(|(start, end)| &text[start..end])
            .collect()
    }

    /// A byte level vocabulary with the extra tokens at ids 256 and above.
    fn synthetic_vocab(extra: &[&str]) -> Vocab {
        let mut tokens: Vec<String> = (0..=255u8).map(token_for_byte).collect();
        tokens.extend(extra.iter().map(|token| (*token).to_string()));
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
    fn the_pre_tokenizer_splits_like_the_pattern() {
        assert_eq!(split("Hello, world!"), ["Hello", ",", " world", "!"]);
        // A word after a run of two spaces takes the second space: the
        // look-ahead of `\s+(?!\S)` refuses to end the run before a word, so
        // the run gives its last space back and ` ?\p{L}+` takes it.
        assert_eq!(split("  double  spaces"), [" ", " double", " ", " spaces"]);
        assert_eq!(split("don't"), ["don", "'t"]);
        assert_eq!(
            split("The capital of France is"),
            ["The", " capital", " of", " France", " is"]
        );
        assert_eq!(split("42 + 7"), ["42", " +", " 7"]);
        assert_eq!(split("  "), ["  "]);
    }

    #[test]
    fn the_rank_is_the_line_after_the_header() {
        let bpe = Bpe::new(b"#version: 0.2\nfirst second\nthird fourth\n")
            .expect("the file is well formed");
        assert_eq!(bpe.rank(b"first", b"second"), Some(0));
        assert_eq!(bpe.rank(b"third", b"fourth"), Some(1));
        assert_eq!(bpe.rank(b"first", b"fourth"), None);
        assert_eq!(bpe.rank(b"second", b"first"), None);
    }

    #[test]
    fn a_merge_joins_its_pair_and_takes_the_lowest_rank() {
        let vocab = synthetic_vocab(&["ab", "Ġc"]);
        let bpe = Bpe::new("#version: 0.2\na b\nĠ c\n".as_bytes()).expect("the merges parse");
        let ids = bpe
            .encode(&vocab, "ab c")
            .expect("every symbol has a token");
        assert_eq!(ids, [256, 257]);
    }

    #[test]
    fn a_symbol_outside_the_vocabulary_is_refused() {
        let vocab = synthetic_vocab(&[]);
        let bpe = Bpe::new(b"a b\n").expect("the merges parse");
        assert!(bpe.encode(&vocab, "ab").is_err());
    }

    #[test]
    fn the_reference_prompt_encodes_to_the_published_ids() {
        let Some((vocab, bpe)) = host_model() else {
            return;
        };
        let ids = bpe
            .encode(&vocab, "The capital of France is")
            .expect("the reference prompt encodes");
        assert_eq!(ids, [504, 3575, 282, 4649, 314]);
    }

    #[test]
    fn the_greeting_encodes_to_the_published_ids() {
        let Some((vocab, bpe)) = host_model() else {
            return;
        };
        let ids = bpe
            .encode(&vocab, "Hello, world!")
            .expect("the greeting encodes");
        assert_eq!(ids, [19556, 28, 905, 17]);
    }

    #[test]
    fn the_ids_decode_back_to_the_bytes_of_the_text() {
        let Some((vocab, bpe)) = host_model() else {
            return;
        };
        for text in [
            "The capital of France is",
            "Hello, world!",
            "  double  spaces",
            "distinct bh 8.6",
        ] {
            let ids = bpe.encode(&vocab, text).expect("the text encodes");
            let decoded = vocab.decode(&ids).expect("the ids decode");
            assert_eq!(
                decoded.bytes,
                text.as_bytes(),
                "{text:?} does not round trip"
            );
            assert_eq!(decoded.outside_alphabet, 0);
        }
    }

    /// The vocabulary and the merges the native comparison uses, when this
    /// host has them (`bench/fetch-model.sh` puts them in the model root).
    fn host_model() -> Option<(Vocab, Bpe)> {
        let directory = host_model_directory()?;
        let vocab = std::fs::read(directory.join("vocab.json")).ok()?;
        let merges = std::fs::read(directory.join("merges.txt")).ok()?;
        let vocab = Vocab::parse(&vocab).ok()?;
        let bpe = Bpe::new(&merges).ok()?;
        Some((vocab, bpe))
    }

    fn host_model_directory() -> Option<PathBuf> {
        if let Ok(root) = std::env::var("MADRUST_INFER_ROOT") {
            let path = Path::new(&root).to_path_buf();
            if path.join("merges.txt").is_file() {
                return Some(path);
            }
        }
        let home = std::env::var_os("HOME")?;
        let path = Path::new(&home)
            .join("madrust")
            .join("artifacts")
            .join("model");
        path.join("merges.txt").is_file().then_some(path)
    }
}
