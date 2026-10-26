// SPDX-License-Identifier: GPL-2.0-only
//! A byte level grammar for tool calls.
//!
//! [`Grammar::tool_call`] builds a deterministic automaton over bytes for the
//! call shape
//!
//! ```text
//! {"tool":"NAME","args":{...}}
//! ```
//!
//! `NAME` is one of the tool names the caller passes, and every argument key is
//! one of [`ARGUMENT_NAMES`]. An argument value is a JSON string, a JSON
//! number, `true`, `false` or `null`. The frame is fixed; the only freedom is
//! in the names, the keys, the values and the number of members, so a sampled
//! sequence that stays inside the automaton is a complete, well formed call.
//!
//! The automaton is built once, from a small NFA with epsilon transitions, by
//! subset construction. [`Grammar::advance`] is a pure byte transition, so the
//! same grammar serves the sampler, the tests and any later tool loop.
//!
//! [`Grammar::allowed_tokens`] turns the byte automaton into a token mask. A
//! token is allowed when its *whole* byte sequence keeps the automaton alive:
//! a token whose first byte is legal but whose later bytes are not is rejected.
//! The result is cached per automaton state, because a vocabulary has about
//! 49k entries and the same state returns many times. The cache assumes one
//! vocabulary per grammar; a later call with a vocabulary of a different size
//! rebuilds it.
//!
//! [`BitSet`] is defined here, not in [`crate::math`], because only this module
//! uses it. It is a `Vec<u64>` with one bit per byte or token id.
//!
//! The construction keeps the invariant the runtime needs: a state that is not
//! complete always has at least one allowed byte. A tool name list and an
//! argument key list must be non-empty; with an empty list the automaton has an
//! empty language and the invariant does not hold.

use core::cell::RefCell;
use std::collections::HashMap;

use crate::vocab::Vocab;

/// Argument keys a tool call may carry.
///
/// The list is fixed at construction so the grammar does not accept an
/// arbitrary key. A caller that needs another key adds it here: the tool loop
/// and the runtime both read this list.
pub const ARGUMENT_NAMES: &[&str] = &[
    "path", "command", "query", "text", "name", "value", "limit", "count", "args",
];

/// A set of small non-negative integers, one bit per element.
///
/// The grammar uses one set per state for the bytes that may follow, and the
/// token mask uses the same type when it is convenient. `set` grows the
/// backing vector as needed; reads never allocate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `bit`.
    pub fn set(&mut self, bit: usize) {
        let word = bit / 64;
        if self.words.len() <= word {
            self.words.resize(word + 1, 0);
        }
        self.words[word] |= 1u64 << (bit % 64);
    }

    /// `true` when `bit` is in the set.
    pub fn contains(&self, bit: usize) -> bool {
        self.words
            .get(bit / 64)
            .is_some_and(|word| word & (1u64 << (bit % 64)) != 0)
    }

    /// `true` when no bit is set.
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&word| word == 0)
    }

    /// Number of set bits.
    pub fn count(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Iterate over the set bits in ascending order.
    pub fn iter(&self) -> BitSetIter<'_> {
        BitSetIter {
            words: &self.words,
            index: 0,
            current: self.words.first().copied().unwrap_or(0),
        }
    }
}

/// Iterator over the set bits of a [`BitSet`].
pub struct BitSetIter<'a> {
    words: &'a [u64],
    index: usize,
    current: u64,
}

impl Iterator for BitSetIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        loop {
            if self.current != 0 {
                let bit = self.current.trailing_zeros() as usize;
                self.current &= self.current - 1;
                return Some(self.index * 64 + bit);
            }
            self.index += 1;
            self.current = *self.words.get(self.index)?;
        }
    }
}

/// An opaque automaton state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct State(usize);

/// A deterministic automaton over bytes.
///
/// The fields are private: a caller moves between states only through
/// [`Grammar::advance`], so it cannot address a state the grammar did not
/// produce.
pub struct Grammar {
    states: Vec<DfaState>,
    empty: BitSet,
    cache: RefCell<Option<TokenCache>>,
}

struct DfaState {
    accepting: bool,
    transitions: Vec<(u8, usize)>,
    allowed: BitSet,
}

struct TokenCache {
    vocab_tokens: usize,
    per_state: Vec<Option<Vec<u32>>>,
}

impl Grammar {
    /// Build the grammar of one tool call over `tool_names`.
    ///
    /// A name must be plain ASCII without a quote or a backslash: it is placed
    /// literally between the quotes of the `tool` value. The list must not be
    /// empty.
    pub fn tool_call(tool_names: &[&str]) -> Grammar {
        let mut nfa = Nfa::new();
        let start = nfa.state();
        let name_start = literal_from(&mut nfa, start, b"{\"tool\":\"");
        let name_end = nfa.state();
        for name in tool_names {
            let (name_state, end) = literal(&mut nfa, name.as_bytes());
            nfa.epsilon(name_start, name_state);
            nfa.epsilon(end, name_end);
        }
        let arguments = literal_from(&mut nfa, name_end, b"\",\"args\":");
        let (object_start, object_end) = object(&mut nfa, ARGUMENT_NAMES);
        nfa.epsilon(arguments, object_start);
        let end = literal_from(&mut nfa, object_end, b"}");
        nfa.accepting[end] = true;

        let states = nfa.determinize(start);
        Grammar {
            states,
            empty: BitSet::new(),
            cache: RefCell::new(None),
        }
    }

    /// The state before the first byte.
    pub fn start(&self) -> State {
        State(0)
    }

    /// Consume `bytes` and return the next state, or `None` when the sequence
    /// leaves the language.
    pub fn advance(&self, state: State, bytes: &[u8]) -> Option<State> {
        let mut current = state.0;
        if current >= self.states.len() {
            return None;
        }
        for &byte in bytes {
            current = transition(&self.states[current].transitions, byte)?;
        }
        Some(State(current))
    }

    /// The bytes that may follow `state`.
    pub fn allowed_bytes(&self, state: State) -> &BitSet {
        match self.states.get(state.0) {
            Some(entry) => &entry.allowed,
            None => &self.empty,
        }
    }

    /// `true` when `state` ends a complete tool call.
    pub fn is_complete(&self, state: State) -> bool {
        self.states
            .get(state.0)
            .is_some_and(|entry| entry.accepting)
    }

    /// The token ids whose whole byte sequence keeps the automaton alive.
    ///
    /// The answer is cached per state. A token with no byte representation in
    /// the byte level alphabet, and the empty token, are never allowed: neither
    /// can consume a byte of the call.
    pub fn allowed_tokens(&self, state: State, vocab: &Vocab) -> Vec<u32> {
        let count = vocab.token_count();
        let mut cache = self.cache.borrow_mut();
        let entry = match cache.as_mut() {
            Some(entry) if entry.vocab_tokens == count => entry,
            _ => {
                *cache = Some(TokenCache {
                    vocab_tokens: count,
                    per_state: vec![None; self.states.len()],
                });
                cache
                    .as_mut()
                    .expect("the cache was just replaced by a full one")
            }
        };
        if let Some(ids) = entry.per_state.get(state.0).and_then(Option::as_ref) {
            return ids.clone();
        }

        let mut ids = Vec::new();
        let mut bytes = Vec::with_capacity(16);
        for index in 0..count {
            let Ok(id) = u32::try_from(index) else {
                break;
            };
            if !vocab.token_bytes_into(id, &mut bytes) || bytes.is_empty() {
                continue;
            }
            if self.advance(state, &bytes).is_some() {
                ids.push(id);
            }
        }
        if let Some(slot) = entry.per_state.get_mut(state.0) {
            *slot = Some(ids.clone());
        }
        ids
    }
}

/// One byte transition of a state.
fn transition(transitions: &[(u8, usize)], byte: u8) -> Option<usize> {
    transitions
        .iter()
        .find_map(|&(value, next)| (value == byte).then_some(next))
}

/// An NFA edge: a consumed byte or an epsilon move.
#[derive(Clone, Copy)]
enum Edge {
    Epsilon(usize),
    Byte(u8, usize),
}

/// The NFA the subset construction starts from.
struct Nfa {
    edges: Vec<Vec<Edge>>,
    accepting: Vec<bool>,
}

impl Nfa {
    fn new() -> Self {
        Self {
            edges: Vec::new(),
            accepting: Vec::new(),
        }
    }

    fn state(&mut self) -> usize {
        let index = self.edges.len();
        self.edges.push(Vec::new());
        self.accepting.push(false);
        index
    }

    fn epsilon(&mut self, from: usize, to: usize) {
        self.edges[from].push(Edge::Epsilon(to));
    }

    fn byte(&mut self, from: usize, byte: u8, to: usize) {
        self.edges[from].push(Edge::Byte(byte, to));
    }

    /// Epsilon closure of a seed set, sorted and deduplicated.
    fn closure(&self, seeds: &[usize]) -> Vec<usize> {
        let mut seen = vec![false; self.edges.len()];
        let mut stack = seeds.to_vec();
        let mut out = Vec::new();
        while let Some(state) = stack.pop() {
            if seen[state] {
                continue;
            }
            seen[state] = true;
            out.push(state);
            for edge in &self.edges[state] {
                if let Edge::Epsilon(to) = *edge {
                    stack.push(to);
                }
            }
        }
        out.sort_unstable();
        out
    }

    /// Subset construction. The resulting start state is always index zero.
    fn determinize(&self, start: usize) -> Vec<DfaState> {
        let initial = self.closure(&[start]);
        let mut sets = vec![initial.clone()];
        let mut lookup: HashMap<Vec<usize>, usize> = HashMap::new();
        lookup.insert(initial, 0);
        let mut states = Vec::new();
        let mut cursor = 0usize;

        while cursor < sets.len() {
            let set = sets[cursor].clone();
            let mut transitions = Vec::new();
            let mut allowed = BitSet::new();
            for byte in 0..=255u8 {
                let mut seeds = Vec::new();
                for &state in &set {
                    for edge in &self.edges[state] {
                        if let Edge::Byte(value, to) = *edge {
                            if value == byte {
                                seeds.push(to);
                            }
                        }
                    }
                }
                if seeds.is_empty() {
                    continue;
                }
                let target = self.closure(&seeds);
                let next = match lookup.get(&target) {
                    Some(&index) => index,
                    None => {
                        let index = sets.len();
                        lookup.insert(target.clone(), index);
                        sets.push(target);
                        index
                    }
                };
                transitions.push((byte, next));
                allowed.set(usize::from(byte));
            }
            let accepting = set.iter().any(|&state| self.accepting[state]);
            states.push(DfaState {
                accepting,
                transitions,
                allowed,
            });
            cursor += 1;
        }
        states
    }
}

/// Add one literal after `from` and return the state after its last byte.
fn literal_from(nfa: &mut Nfa, from: usize, bytes: &[u8]) -> usize {
    let mut current = from;
    for &byte in bytes {
        let next = nfa.state();
        nfa.byte(current, byte, next);
        current = next;
    }
    current
}

/// A fresh literal fragment.
fn literal(nfa: &mut Nfa, bytes: &[u8]) -> (usize, usize) {
    let start = nfa.state();
    let end = literal_from(nfa, start, bytes);
    (start, end)
}

/// A fresh union of fragments.
fn union(nfa: &mut Nfa, fragments: &[(usize, usize)]) -> (usize, usize) {
    let start = nfa.state();
    let end = nfa.state();
    for &(fragment_start, fragment_end) in fragments {
        nfa.epsilon(start, fragment_start);
        nfa.epsilon(fragment_end, end);
    }
    (start, end)
}

/// A JSON string: any byte except `"` and `\`, or one escape sequence.
fn string(nfa: &mut Nfa) -> (usize, usize) {
    let start = nfa.state();
    let end = nfa.state();
    let body = nfa.state();
    nfa.byte(start, b'"', body);
    for byte in 0..=255u8 {
        if byte != b'"' && byte != b'\\' {
            nfa.byte(body, byte, body);
        }
    }
    nfa.byte(body, b'"', end);

    let escape = nfa.state();
    nfa.byte(body, b'\\', escape);
    for &byte in b"\"\\/nrt" {
        nfa.byte(escape, byte, body);
    }
    let (hex_start, hex_end) = hex4(nfa);
    nfa.byte(escape, b'u', hex_start);
    nfa.epsilon(hex_end, body);
    (start, end)
}

/// Exactly four hexadecimal digits.
fn hex4(nfa: &mut Nfa) -> (usize, usize) {
    let start = nfa.state();
    let mut current = start;
    for _ in 0..4 {
        let next = nfa.state();
        for byte in 0..=255u8 {
            if byte.is_ascii_hexdigit() {
                nfa.byte(current, byte, next);
            }
        }
        current = next;
    }
    (start, current)
}

/// A JSON number: `-? (0 | [1-9][0-9]*) (\.[0-9]+)? ([eE][+-]?[0-9]+)?`.
fn number(nfa: &mut Nfa) -> (usize, usize) {
    let start = nfa.state();
    let end = nfa.state();
    let integer = nfa.state();
    let zero = nfa.state();
    let nonzero = nfa.state();

    nfa.epsilon(start, zero);
    nfa.epsilon(start, nonzero);
    let minus = nfa.state();
    nfa.byte(start, b'-', minus);
    nfa.epsilon(minus, zero);
    nfa.epsilon(minus, nonzero);
    nfa.epsilon(zero, integer);
    nfa.epsilon(nonzero, integer);
    for byte in b'0'..=b'9' {
        nfa.byte(nonzero, byte, nonzero);
    }

    let fraction = nfa.state();
    let fraction_digit = nfa.state();
    nfa.byte(integer, b'.', fraction);
    for byte in b'0'..=b'9' {
        nfa.byte(fraction, byte, fraction_digit);
        nfa.byte(fraction_digit, byte, fraction_digit);
    }

    let exponent = nfa.state();
    let exponent_digit = nfa.state();
    for state in [integer, fraction_digit] {
        nfa.byte(state, b'e', exponent);
        nfa.byte(state, b'E', exponent);
    }
    for &byte in b"+-" {
        nfa.byte(exponent, byte, exponent_digit);
    }
    nfa.epsilon(exponent, exponent_digit);
    for byte in b'0'..=b'9' {
        nfa.byte(exponent_digit, byte, exponent_digit);
    }

    nfa.epsilon(integer, end);
    nfa.epsilon(fraction_digit, end);
    nfa.epsilon(exponent_digit, end);
    (start, end)
}

/// One JSON value of an argument.
fn value(nfa: &mut Nfa) -> (usize, usize) {
    let fragments = [
        string(nfa),
        number(nfa),
        literal(nfa, b"true"),
        literal(nfa, b"false"),
        literal(nfa, b"null"),
    ];
    union(nfa, &fragments)
}

/// The argument object: `{}` or members whose keys come from `names`.
fn object(nfa: &mut Nfa, names: &[&str]) -> (usize, usize) {
    let start = nfa.state();
    let end = nfa.state();
    let after_open = nfa.state();
    nfa.byte(start, b'{', after_open);
    nfa.byte(after_open, b'}', end);

    let key_start = nfa.state();
    nfa.byte(after_open, b'"', key_start);

    let quote = nfa.state();
    for name in names {
        let (name_start, name_end) = literal(nfa, name.as_bytes());
        nfa.epsilon(key_start, name_start);
        nfa.epsilon(name_end, quote);
    }
    let colon = nfa.state();
    nfa.byte(quote, b'"', colon);
    let value_start = nfa.state();
    nfa.byte(colon, b':', value_start);

    let (value_start_fragment, value_end_fragment) = value(nfa);
    nfa.epsilon(value_start, value_start_fragment);
    let after_value = nfa.state();
    nfa.epsilon(value_end_fragment, after_value);
    nfa.byte(after_value, b'}', end);
    let comma = nfa.state();
    nfa.byte(after_value, b',', comma);
    nfa.byte(comma, b'"', key_start);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::{BitSet, Grammar};
    use crate::vocab::{token_for_byte, Vocab};

    const CALL: &str = r#"{"tool":"read","args":{"path":"a","limit":1}}"#;

    fn accepts(grammar: &Grammar, bytes: &[u8]) -> bool {
        let mut state = grammar.start();
        for &byte in bytes {
            match grammar.advance(state, &[byte]) {
                Some(next) => state = next,
                None => return false,
            }
        }
        grammar.is_complete(state)
    }

    #[test]
    fn accepts_a_complete_call_and_rejects_a_malformed_one() {
        let grammar = Grammar::tool_call(&["read", "write"]);
        assert!(accepts(&grammar, CALL.as_bytes()));
        assert!(accepts(&grammar, br#"{"tool":"write","args":{}}"#));
        assert!(accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":"a\"b","name":true,"value":null}}"#
        ));

        // Unterminated string: every byte is still legal, but the call is not
        // complete at the end.
        assert!(!accepts(&grammar, br#"{"tool":"read","args":{"path":"a}}"#));
        // A key outside the fixed set.
        assert!(!accepts(&grammar, br#"{"tool":"read","args":{"bad":1}}"#));
        // A tool name outside the choice.
        assert!(!accepts(&grammar, br#"{"tool":"list","args":{}}"#));
        // The outer brace is missing.
        assert!(!accepts(&grammar, br#"{"tool":"read","args":{"path":"a"}"#));
    }

    #[test]
    fn every_non_complete_state_on_a_valid_call_can_continue() {
        let grammar = Grammar::tool_call(&["read", "write"]);
        let vocab = synthetic_vocab(CALL);
        let mut state = grammar.start();
        let mut position = 0usize;

        while position < CALL.len() {
            let allowed = grammar.allowed_tokens(state, &vocab);
            assert!(
                !allowed.is_empty(),
                "empty allowed set at byte {position} of {CALL}"
            );
            assert!(!grammar.allowed_bytes(state).is_empty());
            let (token, length) = allowed
                .iter()
                .filter_map(|&id| {
                    let bytes = vocab.token_bytes(id)?;
                    let end = position.checked_add(bytes.len())?;
                    (end <= CALL.len() && CALL.as_bytes()[position..end] == bytes[..])
                        .then_some((id, bytes.len()))
                })
                .max_by_key(|&(_, length)| length)
                .expect("no allowed token continues the call");
            let bytes = vocab
                .token_bytes(token)
                .expect("an allowed token has bytes");
            state = grammar
                .advance(state, &bytes)
                .expect("an allowed token keeps the automaton alive");
            position += length;
        }
        assert!(grammar.is_complete(state));
        assert!(grammar.allowed_tokens(state, &vocab).is_empty());
    }

    #[test]
    fn every_reachable_non_complete_state_can_continue() {
        let grammar = Grammar::tool_call(&["read", "write"]);
        let mut seen = vec![false; grammar.states.len()];
        let mut stack = vec![0usize];
        while let Some(index) = stack.pop() {
            if seen[index] {
                continue;
            }
            seen[index] = true;
            let state = &grammar.states[index];
            if !state.accepting {
                assert!(
                    !state.allowed.is_empty(),
                    "state {index} has no allowed byte and is not complete"
                );
            }
            for &(_, next) in &state.transitions {
                stack.push(next);
            }
        }
    }

    #[test]
    fn bit_set_iterates_in_order_and_grows() {
        let mut set = BitSet::new();
        assert!(set.is_empty());
        set.set(3);
        set.set(129);
        set.set(64);
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![3, 64, 129]);
        assert_eq!(set.count(), 3);
        assert!(set.contains(64));
        assert!(!set.contains(65));
    }

    /// A vocabulary with one token per byte of `target` plus a few longer
    /// tokens, so the test exercises both one byte and multi byte pieces.
    fn synthetic_vocab(target: &str) -> Vocab {
        let mut tokens: Vec<String> = target
            .bytes()
            .map(token_for_byte)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        for extra in [
            b"{\"tool\":\"".as_slice(),
            b"\",\"args\":{",
            b"read",
            b"write",
            b"path",
            b"command",
            b"limit",
            b"\":\"",
            b"\":",
            b"}}",
        ] {
            tokens.push(extra.iter().map(|&byte| token_for_byte(byte)).collect());
        }
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
}
