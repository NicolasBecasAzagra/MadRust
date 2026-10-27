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
//! number, `true` or `false`; the value of a key is the [`ValueType`] the
//! requirements declare for it. The frame is fixed; the only freedom is in the
//! names, the keys, the values and the number of members, so a sampled
//! sequence that stays inside the automaton is a complete, well formed call.
//!
//! [`Grammar::tool_call_with_requirements`] also takes the keys each tool
//! needs. A call of such a tool is accepted only when its argument object
//! carries every one of those keys, in any order. While a required key is
//! missing the object offers only the missing required keys, so a key that is
//! not required cannot take the place of one; once the requirements are met
//! every key of [`ARGUMENT_NAMES`] stays legal. [`Grammar::tool_call`] is the
//! same grammar with no requirements. A call that is missing an argument the
//! tool reads is not in the language, so it never reaches the tool and never
//! costs a step.
//!
//! The strings of the arguments are bounded: [`Grammar::tool_call`] accepts at
//! most [`DEFAULT_MAX_STRING_LENGTH`] characters in a string value, and
//! [`Grammar::tool_call_with_limit`] takes the bound from the caller. The
//! bound is what keeps a call finite. Without it a string loops over any byte,
//! so a model that opens a string it never closes keeps sampling and spends
//! the whole token budget without producing a call. A tool call carries a
//! path, a command or a short answer, so a small bound costs nothing. The
//! bound applies to the string values only: the tool names and the argument
//! keys are literals, and a number value keeps the width of `number`.
//!
//! One character of a string is one byte outside `"` and `\`, or one escape
//! sequence (`\"`, `\\`, `\/`, `\n`, `\r`, `\t`), even though the escape
//! takes two bytes on the wire.
//!
//! The automaton is built once, from a small NFA with epsilon transitions, by
//! subset construction. [`Grammar::advance`] is a pure byte transition, so the
//! same grammar serves the sampler, the tests and any later tool loop.
//!
//! [`Grammar::allowed_tokens`] turns the byte automaton into a token mask. A
//! token is allowed when its *whole* byte sequence keeps the automaton alive:
//! a token whose first byte is legal but whose later bytes are not is rejected.
//! The result is cached per automaton state, because a vocabulary has about
//! 49k entries and the same state returns many times. The cache belongs to one
//! vocabulary: it carries the fingerprint of the vocabulary it was built for,
//! and a call with another vocabulary rebuilds it.
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

use crate::error::{Error, Result};
use crate::vocab::Vocab;

/// Argument keys a tool call may carry.
///
/// The list is fixed at construction so the grammar does not accept an
/// arbitrary key. A caller that needs another key adds it here: the tool loop
/// and the runtime both read this list.
pub const ARGUMENT_NAMES: &[&str] = &[
    "path", "command", "query", "text", "name", "value", "limit", "count", "args",
];

/// The kind of value an argument carries.
///
/// The grammar offers only a value of the declared kind under the key, so the
/// model cannot spend a step on a call whose argument the tool cannot read,
/// for example a boolean where the tool wants a path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValueType {
    /// A JSON string.
    String,
    /// A JSON number.
    Number,
    /// A JSON boolean, `true` or `false`.
    Bool,
    /// Any of the three.
    Any,
}

/// One argument a tool declares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Argument<'a> {
    /// The key of the member, one of [`ARGUMENT_NAMES`].
    pub key: &'a str,
    /// The kind of value the key carries.
    pub value: ValueType,
    /// `true` when every call of the tool has to carry the key.
    pub required: bool,
}

impl<'a> Argument<'a> {
    /// A key every call of the tool has to carry, with the value it takes.
    pub const fn required(key: &'a str, value: ValueType) -> Self {
        Self {
            key,
            value,
            required: true,
        }
    }

    /// A key a call may carry, with the value it takes.
    pub const fn optional(key: &'a str, value: ValueType) -> Self {
        Self {
            key,
            value,
            required: false,
        }
    }
}

/// The arguments of each tool, by tool name.
///
/// A tool that is absent from the table needs no argument. Every key must be
/// one of [`ARGUMENT_NAMES`]: the grammar cannot accept a key it does not
/// know, so a declaration outside that list is an error at construction
/// instead of a grammar that accepts nothing. A key the table does not declare
/// for a tool takes any value. The table order carries no meaning: a call may
/// list the required keys in any order, and once they are all present it may
/// carry any accepted key.
pub type Requirements<'a> = &'a [(&'a str, &'a [Argument<'a>])];

/// Characters a string value of a tool call may hold under
/// [`Grammar::tool_call`].
///
/// A tool call carries a path, a command, a number or a short answer, never a
/// document. 128 characters hold the longest path of the tool registry and a
/// sentence of the `done` answer with margin, and the automaton grows by two
/// states per character, so the default stays small. A caller that needs
/// another bound uses [`Grammar::tool_call_with_limit`].
pub const DEFAULT_MAX_STRING_LENGTH: usize = 128;

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
    /// The fingerprint of the vocabulary the mask was built for.
    vocab_fingerprint: u64,
    per_state: Vec<Option<Vec<u32>>>,
}

impl Grammar {
    /// Build the grammar of one tool call over `tool_names`.
    ///
    /// The string values of the arguments hold at most
    /// [`DEFAULT_MAX_STRING_LENGTH`] characters. Use
    /// [`Grammar::tool_call_with_limit`] to choose another bound; the bound is
    /// what makes a call finite, because an unbounded string lets a model
    /// sample until the token budget runs out.
    ///
    /// A name must be plain ASCII without a quote or a backslash: it is placed
    /// literally between the quotes of the `tool` value. The list must not be
    /// empty, because a grammar without a name has an empty language and every
    /// step would stop with nothing to say.
    pub fn tool_call(tool_names: &[&str]) -> Result<Grammar> {
        Self::tool_call_with_requirements(tool_names, &[])
    }

    /// Build the grammar of one tool call whose argument strings hold at most
    /// `max_length` characters.
    ///
    /// One character is one byte outside `"` and `\`, or one escape sequence
    /// (`\"`, `\\`, `\/`, `\n`, `\r`, `\t`), which takes two bytes on the wire.
    /// A bound of zero accepts only the empty string. The automaton builds the
    /// string as a chain of `max_length` positions, so the state count grows
    /// by about two states per character and never by subset blow up; a call
    /// whose string is one character longer has no legal byte and generation
    /// stops.
    ///
    /// The bound applies to the string values only. The tool names and the
    /// argument keys are literals, and a number value keeps the width of the
    /// JSON number production.
    ///
    /// A name must be plain ASCII without a quote or a backslash: it is placed
    /// literally between the quotes of the `tool` value. The list must not be
    /// empty, because a grammar without a name has an empty language and every
    /// step would stop with nothing to say.
    pub fn tool_call_with_limit(tool_names: &[&str], max_length: usize) -> Result<Grammar> {
        Self::tool_call_with_requirements_and_limit(tool_names, &[], max_length)
    }

    /// Build the grammar of one tool call where each tool accepts only a call
    /// that carries the arguments [`Requirements`] lists for it.
    ///
    /// The string values of the arguments hold at most
    /// [`DEFAULT_MAX_STRING_LENGTH`] characters; use
    /// [`Grammar::tool_call_with_requirements_and_limit`] for another bound.
    /// The names and the rest of the construction follow [`Grammar::tool_call`].
    ///
    /// The object of a call lists the required keys of its tool first, in any
    /// order, and only then any other key of [`ARGUMENT_NAMES`]; the object
    /// cannot close before every required key is there. Under each key the
    /// grammar offers only the value of the declared [`ValueType`], or any of
    /// the three when the key is not declared. Every key of `requirements` must
    /// be one of [`ARGUMENT_NAMES`]. A declaration the grammar cannot carry is
    /// an error, not a tool that accepts nothing: an empty language would stop
    /// every step of the loop. A tool the table does not name, and a name in
    /// the table that is not one of `tool_names`, needs no argument.
    pub fn tool_call_with_requirements(
        tool_names: &[&str],
        requirements: Requirements<'_>,
    ) -> Result<Grammar> {
        Self::tool_call_with_requirements_and_limit(
            tool_names,
            requirements,
            DEFAULT_MAX_STRING_LENGTH,
        )
    }

    /// Build the grammar of one tool call with both the argument requirements
    /// and the string bound of the caller.
    ///
    /// The automaton keeps one copy of the value frame per subset of the keys
    /// a tool needs and per value type, so a tool that needs `n` keys costs
    /// `2^n` times the value frame; the tool registry needs at most two keys
    /// per tool, all of them strings.
    pub fn tool_call_with_requirements_and_limit(
        tool_names: &[&str],
        requirements: Requirements<'_>,
        max_length: usize,
    ) -> Result<Grammar> {
        validate_names(tool_names)?;
        validate_requirements(requirements)?;
        let mut nfa = Nfa::new();
        let start = nfa.state();
        let name_start = literal_from(&mut nfa, start, b"{\"tool\":\"");
        let end = nfa.state();
        // One frame per distinct argument table. Two tools that declare the
        // same arguments share the frame, so a grammar without requirements
        // keeps the shape it had before the requirements existed.
        let mut frames: Vec<(Vec<Argument<'_>>, usize)> = Vec::new();
        for &name in tool_names {
            let arguments = declared_arguments(requirements, name);
            let suffix = match frames
                .iter()
                .position(|(declared, _)| *declared == arguments)
            {
                Some(index) => frames[index].1,
                None => {
                    let suffix = nfa.state();
                    let open = literal_from(&mut nfa, suffix, b"\",\"args\":");
                    let (object_start, object_end) =
                        object(&mut nfa, ARGUMENT_NAMES, &arguments, max_length);
                    nfa.epsilon(open, object_start);
                    let finish = literal_from(&mut nfa, object_end, b"}");
                    nfa.epsilon(finish, end);
                    frames.push((arguments, suffix));
                    suffix
                }
            };
            let (name_state, name_end) = literal(&mut nfa, name.as_bytes());
            nfa.epsilon(name_start, name_state);
            nfa.epsilon(name_end, suffix);
        }
        nfa.accepting[end] = true;

        let states = nfa.determinize(start);
        Ok(Grammar {
            states,
            empty: BitSet::new(),
            cache: RefCell::new(None),
        })
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
        let fingerprint = vocab.fingerprint();
        let mut cache = self.cache.borrow_mut();
        let entry = match cache.as_mut() {
            Some(entry) if entry.vocab_fingerprint == fingerprint => entry,
            _ => {
                *cache = Some(TokenCache {
                    vocab_fingerprint: fingerprint,
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

/// Check that every tool name can be placed literally in the call.
fn validate_names(tool_names: &[&str]) -> Result<()> {
    if tool_names.is_empty() {
        return Err(Error::new(
            "grammar: a tool call needs at least one tool name",
        ));
    }
    for name in tool_names {
        let plain = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\');
        if !plain {
            return Err(Error::new(format!(
                "grammar: the tool name {name:?} is not plain ASCII"
            )));
        }
    }
    Ok(())
}

/// Check that every declared key is one the grammar can carry.
fn validate_requirements(requirements: Requirements<'_>) -> Result<()> {
    for &(name, arguments) in requirements {
        for argument in arguments {
            if !ARGUMENT_NAMES.contains(&argument.key) {
                return Err(Error::new(format!(
                    "grammar: the tool {name:?} declares the key {:?}, which is not an argument name",
                    argument.key
                )));
            }
        }
    }
    Ok(())
}

/// The arguments `tool` declares, with no repeated key.
///
/// Entries for the same tool merge. The first declaration of a key sets its
/// value type, and the key is required when any declaration says so.
fn declared_arguments<'a>(requirements: Requirements<'a>, tool: &str) -> Vec<Argument<'a>> {
    let mut declared: Vec<Argument<'a>> = Vec::new();
    for &(name, arguments) in requirements {
        if name != tool {
            continue;
        }
        for argument in arguments {
            match declared.iter_mut().find(|entry| entry.key == argument.key) {
                Some(entry) => entry.required |= argument.required,
                None => declared.push(*argument),
            }
        }
    }
    declared
}

/// The value `key` takes for this tool: the declared type, or any of the three
/// when the table does not name the key.
fn value_type(arguments: &[Argument<'_>], key: &str) -> ValueType {
    arguments
        .iter()
        .find(|argument| argument.key == key)
        .map_or(ValueType::Any, |argument| argument.value)
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

/// A JSON string of at most `max_length` characters: a byte outside `"` and
/// `\`, or one escape sequence.
///
/// The body is a chain of `max_length` positions, so a string one character
/// too long has no next byte. A position also closes the string, so every
/// reachable position can reach a complete call.
fn string(nfa: &mut Nfa, max_length: usize) -> (usize, usize) {
    let start = nfa.state();
    let end = nfa.state();
    let mut position = nfa.state();
    nfa.byte(start, b'"', position);
    for _ in 0..max_length {
        let next = nfa.state();
        for byte in 0x20..=255u8 {
            // A control byte below 0x20 is not allowed in a JSON string, and
            // the parser of the runtime refuses it, so the grammar must not
            // offer it.
            if byte != b'"' && byte != b'\\' {
                nfa.byte(position, byte, next);
            }
        }
        let escape = nfa.state();
        nfa.byte(position, b'\\', escape);
        for &byte in b"\"\\/nrt" {
            nfa.byte(escape, byte, next);
        }
        nfa.byte(position, b'"', end);
        position = next;
    }
    nfa.byte(position, b'"', end);
    // The `\u` escape is not offered: a code point needs a surrogate pair
    // when it is above the basic plane, and the parser of the runtime only
    // accepts the pairs. A grammar that offers the escape and a parser that
    // refuses half of it is a step that ends without an answer.
    (start, end)
}

/// A JSON number: `-? (0 | [1-9][0-9]*) (\.[0-9]+)? ([eE][+-]?[0-9]+)?`.
fn number(nfa: &mut Nfa) -> (usize, usize) {
    let start = nfa.state();
    let end = nfa.state();
    let integer = nfa.state();
    let zero = nfa.state();
    let nonzero = nfa.state();

    // A digit has to be consumed before the integer part: `zero` and `nonzero`
    // are reached by a byte, never by an epsilon, so an empty value or a value
    // that starts with `.` or `e` is not in the language.
    nfa.byte(start, b'0', zero);
    let minus = nfa.state();
    nfa.byte(start, b'-', minus);
    nfa.byte(minus, b'0', zero);
    for byte in b'1'..=b'9' {
        nfa.byte(start, byte, nonzero);
        nfa.byte(minus, byte, nonzero);
        nfa.byte(nonzero, byte, nonzero);
    }
    nfa.byte(nonzero, b'0', nonzero);
    nfa.epsilon(zero, integer);
    nfa.epsilon(nonzero, integer);

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

/// One JSON value of the declared kind.
///
/// [`ValueType::Any`] is the union of the other three: a null value is not in
/// the language, because a tool argument is a path, a number or a short answer,
/// and no tool reads a null.
fn typed_value(nfa: &mut Nfa, kind: ValueType, max_length: usize) -> (usize, usize) {
    match kind {
        ValueType::String => string(nfa, max_length),
        ValueType::Number => number(nfa),
        ValueType::Bool => {
            let fragments = [literal(nfa, b"true"), literal(nfa, b"false")];
            union(nfa, &fragments)
        }
        ValueType::Any => {
            let fragments = [
                string(nfa, max_length),
                number(nfa),
                literal(nfa, b"true"),
                literal(nfa, b"false"),
            ];
            union(nfa, &fragments)
        }
    }
}

/// The argument object: `{}`, or the missing required keys of `arguments`
/// followed by any key of `names`.
///
/// A mask records which required keys the members already carried. While the
/// mask is not full, the only members the object offers are the required keys
/// it still misses, in any order, so a key that is not required cannot take
/// the place of one that is. The object closes only at the full mask, and the
/// keys of `names` stay legal from there on. The first member follows the
/// opening brace without a comma; every later member starts with one.
///
/// One value frame is built per next mask and value type: every key that lands
/// on the same mask with the same type shares the frame, and the frame returns
/// to the state after a value of that mask.
fn object(
    nfa: &mut Nfa,
    names: &[&str],
    arguments: &[Argument<'_>],
    max_length: usize,
) -> (usize, usize) {
    let required: Vec<&str> = arguments
        .iter()
        .filter(|argument| argument.required)
        .map(|argument| argument.key)
        .collect();
    let masks = 1usize << required.len();
    let full = masks - 1;
    let start = nfa.state();
    let end = nfa.state();
    let first = nfa.state();
    nfa.byte(start, b'{', first);
    let after_value: Vec<usize> = (0..masks).map(|_| nfa.state()).collect();
    // `{}` is the whole object only when the tool needs nothing.
    if full == 0 {
        nfa.byte(first, b'}', end);
    }

    for (mask, &after) in after_value.iter().enumerate() {
        // One key position per mask, shared by the first member and by a
        // member after a comma.
        let key_start = nfa.state();
        if mask == 0 {
            nfa.byte(first, b'"', key_start);
        }
        let comma = nfa.state();
        nfa.byte(after, b',', comma);
        nfa.byte(comma, b'"', key_start);
        if mask == full {
            nfa.byte(after, b'}', end);
        }

        // While a required key is missing, it and the other missing keys are
        // the only members. The full mask carries every accepted key.
        let members: Vec<(&str, usize)> = if mask == full {
            names.iter().map(|&name| (name, full)).collect()
        } else {
            required
                .iter()
                .enumerate()
                .filter(|&(bit, _)| mask & (1 << bit) == 0)
                .map(|(bit, &key)| (key, mask | (1 << bit)))
                .collect()
        };
        // One closing quote and value frame per next mask and value type:
        // every key that lands on the same pair shares them.
        let mut frames: Vec<((usize, ValueType), usize)> = Vec::new();
        for (name, next) in members {
            let kind = value_type(arguments, name);
            let colon = match frames.iter().find(|(key, _)| *key == (next, kind)) {
                Some(&(_, colon)) => colon,
                None => {
                    let colon = nfa.state();
                    let (value_start, value_end) = typed_value(nfa, kind, max_length);
                    nfa.byte(colon, b':', value_start);
                    nfa.epsilon(value_end, after_value[next]);
                    frames.push(((next, kind), colon));
                    colon
                }
            };
            let (key_state, key_end) = literal(nfa, name.as_bytes());
            nfa.epsilon(key_start, key_state);
            nfa.byte(key_end, b'"', colon);
        }
    }
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::{Argument, BitSet, Grammar, Requirements, ValueType, DEFAULT_MAX_STRING_LENGTH};
    use crate::vocab::{token_for_byte, Vocab};

    const CALL: &str = r#"{"tool":"read","args":{"path":"a","limit":1}}"#;

    /// A requirement table for the tests: `read` and `write` want strings,
    /// `number` wants a number, `flag` a boolean, `listing` types an optional
    /// key, and `plain` declares nothing.
    const REQUIREMENTS: Requirements<'_> = &[
        ("read", &[Argument::required("path", ValueType::String)]),
        (
            "write",
            &[
                Argument::required("path", ValueType::String),
                Argument::required("text", ValueType::String),
            ],
        ),
        ("plain", &[]),
        ("number", &[Argument::required("limit", ValueType::Number)]),
        ("flag", &[Argument::required("count", ValueType::Bool)]),
        ("listing", &[Argument::optional("limit", ValueType::Number)]),
    ];

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

    /// A complete call whose single string value holds `length` characters.
    fn call_with_string(length: usize) -> Vec<u8> {
        format!(
            "{{\"tool\":\"read\",\"args\":{{\"path\":\"{}\"}}}}",
            "a".repeat(length)
        )
        .into_bytes()
    }

    #[test]
    fn accepts_a_complete_call_and_rejects_a_malformed_one() {
        let grammar = Grammar::tool_call(&["read", "write"]).expect("the names are plain");
        assert!(accepts(&grammar, CALL.as_bytes()));
        assert!(accepts(&grammar, br#"{"tool":"write","args":{}}"#));
        assert!(accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":"a\"b","name":true,"value":false}}"#
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
    fn a_call_with_every_required_key_is_accepted() {
        let grammar = Grammar::tool_call_with_requirements(&["read", "write"], REQUIREMENTS)
            .expect("every required key is an accepted key");
        assert!(accepts(&grammar, br#"{"tool":"read","args":{"path":"a"}}"#));
        assert!(accepts(
            &grammar,
            br#"{"tool":"write","args":{"path":"a","text":"b"}}"#
        ));
        // Extra accepted keys do not hurt.
        assert!(accepts(
            &grammar,
            br#"{"tool":"write","args":{"path":"a","text":"b","limit":1}}"#
        ));
    }

    #[test]
    fn a_call_missing_a_required_key_is_rejected() {
        let grammar = Grammar::tool_call_with_requirements(&["read", "write"], REQUIREMENTS)
            .expect("every required key is an accepted key");
        assert!(!accepts(&grammar, br#"{"tool":"read","args":{}}"#));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"text":"a"}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"write","args":{"path":"a"}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"write","args":{"text":"b"}}"#
        ));
    }

    #[test]
    fn the_order_of_the_required_keys_does_not_matter() {
        let grammar = Grammar::tool_call_with_requirements(&["write"], REQUIREMENTS)
            .expect("every required key is an accepted key");
        assert!(accepts(
            &grammar,
            br#"{"tool":"write","args":{"path":"a","text":"b"}}"#
        ));
        assert!(accepts(
            &grammar,
            br#"{"tool":"write","args":{"text":"b","path":"a"}}"#
        ));
        // A key of the accepted list after the required ones, and a repeated
        // required key, still close the call.
        assert!(accepts(
            &grammar,
            br#"{"tool":"write","args":{"text":"b","path":"a","count":1,"text":"c"}}"#
        ));
    }

    #[test]
    fn the_open_brace_only_offers_a_missing_required_key() {
        let grammar = Grammar::tool_call_with_requirements(&["read"], REQUIREMENTS)
            .expect("path is an accepted key");
        let after_open = grammar
            .advance(grammar.start(), br#"{"tool":"read","args":{"#)
            .expect("the frame is legal");
        assert_eq!(
            grammar.allowed_bytes(after_open).iter().collect::<Vec<_>>(),
            vec![usize::from(b'"')],
            "a comma or a closing brace right after the opening brace is not JSON"
        );
        // Only the required key starts here, not `query` or another key of the
        // accepted list.
        let after_quote = grammar
            .advance(after_open, b"\"")
            .expect("the quote of a key is legal");
        assert_eq!(
            grammar
                .allowed_bytes(after_quote)
                .iter()
                .collect::<Vec<_>>(),
            vec![usize::from(b'p')],
            "a key that is not required must not take the place of `path`"
        );
    }

    #[test]
    fn the_object_closes_only_when_every_required_key_is_present() {
        let grammar = Grammar::tool_call_with_requirements(&["write"], REQUIREMENTS)
            .expect("both keys are accepted keys");
        let after_path = grammar
            .advance(grammar.start(), br#"{"tool":"write","args":{"path":"a""#)
            .expect("the first member is legal");
        let allowed = grammar.allowed_bytes(after_path);
        assert!(allowed.contains(usize::from(b',')));
        assert!(
            !allowed.contains(usize::from(b'}')),
            "the object closed before `text`"
        );
        let after_text = grammar
            .advance(after_path, br#","text":"b""#)
            .expect("the second member is legal");
        assert!(
            grammar
                .allowed_bytes(after_text)
                .contains(usize::from(b'}')),
            "the object does not close after every required key"
        );
    }

    #[test]
    fn a_member_starts_with_a_comma_only_after_another_member() {
        let grammar = Grammar::tool_call_with_requirements(&["read"], REQUIREMENTS)
            .expect("path is an accepted key");
        // A comma right after the opening brace, and a member with no comma
        // after the one before it, are the two malformed shapes of the run.
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{,"path":"a"}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":"a""path":"b"}}"#
        ));
        assert!(accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":"a","path":"b"}}"#
        ));
    }

    #[test]
    fn a_key_that_is_not_required_waits_for_the_required_ones() {
        let grammar = Grammar::tool_call_with_requirements(&["read"], REQUIREMENTS)
            .expect("path is an accepted key");
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"query":false}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"query":false,"path":"a"}}"#
        ));
        assert!(accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":"a","query":false,"limit":1}}"#
        ));
    }

    #[test]
    fn a_tool_without_requirements_accepts_an_empty_object() {
        let grammar = Grammar::tool_call_with_requirements(&["plain", "read"], REQUIREMENTS)
            .expect("every required key is an accepted key");
        assert!(accepts(&grammar, br#"{"tool":"plain","args":{}}"#));
        // The empty object stays legal for the tool that needs nothing, and
        // only for that tool.
        assert!(!accepts(&grammar, br#"{"tool":"read","args":{}}"#));
    }

    #[test]
    fn a_requirement_outside_the_argument_names_is_an_error() {
        let error = match Grammar::tool_call_with_requirements(
            &["read"],
            &[("read", &[Argument::required("body", ValueType::String)])],
        ) {
            Ok(_) => panic!("`body` is not an accepted argument key"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("body"), "{error}");
    }

    #[test]
    fn a_string_key_takes_only_a_string() {
        let grammar = Grammar::tool_call_with_requirements(&["read"], REQUIREMENTS)
            .expect("path is an accepted key");
        assert!(accepts(&grammar, br#"{"tool":"read","args":{"path":"a"}}"#));
        assert!(!accepts(&grammar, br#"{"tool":"read","args":{"path":1}}"#));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":true}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":{"a":1}}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":null}}"#
        ));
    }

    #[test]
    fn a_number_key_takes_only_a_number() {
        let grammar = Grammar::tool_call_with_requirements(&["number"], REQUIREMENTS)
            .expect("limit is an accepted key");
        assert!(accepts(
            &grammar,
            br#"{"tool":"number","args":{"limit":1}}"#
        ));
        assert!(accepts(
            &grammar,
            br#"{"tool":"number","args":{"limit":-2.5e3}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"number","args":{"limit":"1"}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"number","args":{"limit":true}}"#
        ));
    }

    #[test]
    fn a_number_value_is_a_complete_json_number() {
        let grammar = Grammar::tool_call_with_requirements(&["number"], REQUIREMENTS)
            .expect("limit is an accepted key");
        for accepted in [
            br#"{"tool":"number","args":{"limit":0}}"#.as_slice(),
            br#"{"tool":"number","args":{"limit":-0.5e+3}}"#,
            br#"{"tool":"number","args":{"limit":12}}"#,
        ] {
            assert!(
                accepts(&grammar, accepted),
                "{} is a JSON number",
                String::from_utf8_lossy(accepted)
            );
        }
        for rejected in [
            br#"{"tool":"number","args":{"limit":}}"#.as_slice(),
            br#"{"tool":"number","args":{"limit":.5}}"#,
            br#"{"tool":"number","args":{"limit":05}}"#,
            br#"{"tool":"number","args":{"limit":-}}"#,
            br#"{"tool":"number","args":{"limit":1.}}"#,
            br#"{"tool":"number","args":{"limit":e5}}"#,
        ] {
            assert!(
                !accepts(&grammar, rejected),
                "{} is not a JSON number",
                String::from_utf8_lossy(rejected)
            );
        }
    }

    #[test]
    fn a_boolean_key_takes_only_a_boolean() {
        let grammar = Grammar::tool_call_with_requirements(&["flag"], REQUIREMENTS)
            .expect("count is an accepted key");
        assert!(accepts(
            &grammar,
            br#"{"tool":"flag","args":{"count":true}}"#
        ));
        assert!(accepts(
            &grammar,
            br#"{"tool":"flag","args":{"count":false}}"#
        ));
        assert!(!accepts(&grammar, br#"{"tool":"flag","args":{"count":1}}"#));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"flag","args":{"count":"true"}}"#
        ));
    }

    #[test]
    fn a_key_with_no_type_takes_the_three() {
        let grammar = Grammar::tool_call_with_requirements(&["plain"], REQUIREMENTS)
            .expect("the table is accepted");
        assert!(accepts(&grammar, br#"{"tool":"plain","args":{}}"#));
        assert!(accepts(
            &grammar,
            br#"{"tool":"plain","args":{"query":"a"}}"#
        ));
        assert!(accepts(&grammar, br#"{"tool":"plain","args":{"query":1}}"#));
        assert!(accepts(
            &grammar,
            br#"{"tool":"plain","args":{"query":false}}"#
        ));
    }

    #[test]
    fn an_optional_key_keeps_the_type_it_is_given() {
        let grammar = Grammar::tool_call_with_requirements(&["listing"], REQUIREMENTS)
            .expect("the table is accepted");
        assert!(accepts(&grammar, br#"{"tool":"listing","args":{}}"#));
        assert!(accepts(
            &grammar,
            br#"{"tool":"listing","args":{"limit":7}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"listing","args":{"limit":"7"}}"#
        ));
    }

    #[test]
    fn the_value_of_a_string_key_offers_no_other_type() {
        let grammar = Grammar::tool_call_with_requirements(&["read"], REQUIREMENTS)
            .expect("path is an accepted key");
        let after_colon = grammar
            .advance(grammar.start(), br#"{"tool":"read","args":{"path":"#)
            .expect("the key and its colon are legal");
        assert_eq!(
            grammar
                .allowed_bytes(after_colon)
                .iter()
                .collect::<Vec<_>>(),
            vec![usize::from(b'"')],
            "a number, a boolean or a null must not start the value of `path`"
        );
    }

    #[test]
    fn every_non_complete_state_of_a_requiring_grammar_can_finish() {
        let grammar = Grammar::tool_call_with_requirements(&["read", "write"], REQUIREMENTS)
            .expect("every required key is an accepted key");
        let mut reverse = vec![Vec::new(); grammar.states.len()];
        for (index, state) in grammar.states.iter().enumerate() {
            for &(_, next) in &state.transitions {
                reverse[next].push(index);
            }
        }
        let mut reaches_end = vec![false; grammar.states.len()];
        let mut stack: Vec<usize> = (0..grammar.states.len())
            .filter(|&index| grammar.states[index].accepting)
            .collect();
        for &index in &stack {
            reaches_end[index] = true;
        }
        while let Some(index) = stack.pop() {
            for &previous in &reverse[index] {
                if !reaches_end[previous] {
                    reaches_end[previous] = true;
                    stack.push(previous);
                }
            }
        }

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
                assert!(
                    reaches_end[index],
                    "state {index} cannot reach a complete call"
                );
            }
            for &(_, next) in &state.transitions {
                stack.push(next);
            }
        }
    }

    #[test]
    fn every_non_complete_state_on_a_valid_call_can_continue() {
        let grammar = Grammar::tool_call(&["read", "write"]).expect("the names are plain");
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
        let grammar = Grammar::tool_call(&["read", "write"]).expect("the names are plain");
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
    fn a_string_of_exactly_the_bound_is_accepted() {
        let grammar = Grammar::tool_call_with_limit(&["read"], 8).expect("the names are plain");
        assert!(accepts(&grammar, &call_with_string(8)));
        assert!(!accepts(&grammar, &call_with_string(9)));
    }

    #[test]
    fn an_escape_counts_as_one_character() {
        let grammar = Grammar::tool_call_with_limit(&["read"], 2).expect("the names are plain");
        // Four wire bytes, two characters.
        assert!(accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":"\n\t"}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"path":"\n\t\n"}}"#
        ));
    }

    #[test]
    fn several_arguments_each_within_the_bound_are_accepted() {
        let grammar = Grammar::tool_call_with_limit(&["write"], 8).expect("the names are plain");
        assert!(accepts(
            &grammar,
            br#"{"tool":"write","args":{"path":"a/b.txt","text":"hola que","count":7}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"write","args":{"path":"a/b.txt","text":"hola que tal"}}"#
        ));
    }

    #[test]
    fn the_bound_does_not_touch_the_tool_name_or_the_keys() {
        // `read` and `command` are longer than the bound: both are literals,
        // so the bound never applies to them.
        let grammar = Grammar::tool_call_with_limit(&["read"], 3).expect("the names are plain");
        assert!(accepts(
            &grammar,
            br#"{"tool":"read","args":{"command":"ls"}}"#
        ));
        assert!(!accepts(
            &grammar,
            br#"{"tool":"read","args":{"command":"ls -l"}}"#
        ));
    }

    #[test]
    fn every_reachable_non_complete_state_reaches_a_complete_call() {
        for max_length in [0usize, 1, 4, DEFAULT_MAX_STRING_LENGTH] {
            let grammar = Grammar::tool_call_with_limit(&["read", "write"], max_length)
                .expect("the names are plain");
            let mut reverse = vec![Vec::new(); grammar.states.len()];
            for (index, state) in grammar.states.iter().enumerate() {
                for &(_, next) in &state.transitions {
                    reverse[next].push(index);
                }
            }
            let mut reaches_end = vec![false; grammar.states.len()];
            let mut stack: Vec<usize> = (0..grammar.states.len())
                .filter(|&index| grammar.states[index].accepting)
                .collect();
            for &index in &stack {
                reaches_end[index] = true;
            }
            while let Some(index) = stack.pop() {
                for &previous in &reverse[index] {
                    if !reaches_end[previous] {
                        reaches_end[previous] = true;
                        stack.push(previous);
                    }
                }
            }

            let mut seen = vec![false; grammar.states.len()];
            let mut stack = vec![0usize];
            while let Some(index) = stack.pop() {
                if seen[index] {
                    continue;
                }
                seen[index] = true;
                if !grammar.states[index].accepting {
                    assert!(
                        reaches_end[index],
                        "state {index} of the bound {max_length} cannot reach a complete call"
                    );
                }
                for &(_, next) in &grammar.states[index].transitions {
                    stack.push(next);
                }
            }
        }
    }

    #[test]
    fn the_state_count_stays_small_for_the_default_bound() {
        let grammar = Grammar::tool_call(&["read", "write"]).expect("the names are plain");
        assert!(
            grammar.states.len() < 700,
            "{} states for the default bound of {DEFAULT_MAX_STRING_LENGTH} characters",
            grammar.states.len()
        );
        // The bound is a chain: each character adds one position state and one
        // escape state, never a subset blow up.
        let limited = Grammar::tool_call_with_limit(&["read", "write"], 64).expect("names");
        assert_eq!(
            grammar.states.len() - limited.states.len(),
            2 * (DEFAULT_MAX_STRING_LENGTH - 64)
        );
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
