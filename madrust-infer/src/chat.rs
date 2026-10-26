// SPDX-License-Identifier: GPL-2.0-only
//! The SmolLM2 chat prompt, built from plain strings.
//!
//! The runtime does not tokenize prompts; it receives token ids. This module
//! covers the other half of a tool loop: it writes the text of a chat turn with
//! the markers the SmolLM2 chat template uses, so a caller can encode the
//! string with any byte level tokenizer and feed the ids to the runtime.
//!
//! Every turn is one block:
//!
//! ```text
//! <|im_start|>role
//! text
//! <|im_end|>
//! ```
//!
//! [`tool_prompt`] writes the system block (with the available tool names when
//! the list is not empty), the history in order, the current user turn, and
//! finally `<|im_start|>assistant\n` without a closing marker, so the model
//! continues as the assistant. That is the shape the template produces for a
//! generation request.
//!
//! [`missing_markers`] checks that the vocabulary carries the two marker
//! strings as single tokens. A tokenizer that splits a marker into several
//! tokens is not detectable here, and a vocabulary that lacks a marker makes
//! the prompt ambiguous; the caller reports the missing names before it runs.

use crate::vocab::Vocab;

/// Opening marker of one chat turn.
pub const IM_START: &str = "<|im_start|>";

/// Closing marker of one chat turn.
pub const IM_END: &str = "<|im_end|>";

/// The markers a SmolLM2 chat prompt needs in the vocabulary.
pub const MARKERS: [&str; 2] = [IM_START, IM_END];

/// Who wrote one turn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// Instruction block at the top of the prompt.
    System,
    /// A turn from the user.
    User,
    /// A turn from the model.
    Assistant,
}

impl Role {
    /// The name the chat template writes after `<|im_start|>`.
    pub fn name(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// Build the prompt of one chat turn.
///
/// The system block comes first. When `tools` is not empty its names are
/// appended to the system text on one `Available tools: ...` line, separated by
/// `, `. `history` is then written in order, each entry as its own block, and
/// `user` is the current turn. The prompt ends with the assistant marker so the
/// model continues the conversation.
///
/// The function is pure string assembly: it does not escape the text, so the
/// caller must not put a chat marker inside a message.
pub fn tool_prompt(system: &str, user: &str, tools: &[&str], history: &[(Role, String)]) -> String {
    let mut prompt = String::new();
    push_block(&mut prompt, Role::System, &system_text(system, tools));
    for (role, text) in history {
        push_block(&mut prompt, *role, text);
    }
    push_block(&mut prompt, Role::User, user);
    prompt.push_str(IM_START);
    prompt.push_str(Role::Assistant.name());
    prompt.push('\n');
    prompt
}

/// The names of the markers the vocabulary does not carry.
///
/// A marker counts as present only when the vocabulary has it as one exact
/// token string. The result keeps the order of [`MARKERS`], so the caller can
/// print it as a stable list.
pub fn missing_markers(vocab: &Vocab) -> Vec<&'static str> {
    MARKERS
        .into_iter()
        .filter(|marker| vocab.find_token(marker).is_none())
        .collect()
}

/// Append one `role` block, including its closing marker and newline.
fn push_block(prompt: &mut String, role: Role, text: &str) {
    prompt.push_str(IM_START);
    prompt.push_str(role.name());
    prompt.push('\n');
    prompt.push_str(text);
    prompt.push('\n');
    prompt.push_str(IM_END);
    prompt.push('\n');
}

/// The system text with the tool list appended when there is one.
fn system_text(system: &str, tools: &[&str]) -> String {
    if tools.is_empty() {
        return system.to_string();
    }
    let mut text = String::from(system);
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str("Available tools: ");
    text.push_str(&tools.join(", "));
    text
}

#[cfg(test)]
mod tests {
    use super::{missing_markers, tool_prompt, Role, IM_END, IM_START};
    use crate::vocab::Vocab;

    #[test]
    fn the_prompt_has_the_smolm2_markers_and_ends_on_the_assistant() {
        let history = [
            (Role::User, String::from("hello")),
            (Role::Assistant, String::from("hi")),
        ];
        let prompt = tool_prompt(
            "You are a tool user.",
            "read the file",
            &["read", "write"],
            &history,
        );

        assert!(prompt.starts_with(&format!("{IM_START}system\n")));
        assert!(prompt.contains("You are a tool user.\nAvailable tools: read, write\n"));
        assert!(prompt.contains(&format!("{IM_START}user\nhello\n{IM_END}\n")));
        assert!(prompt.contains(&format!("{IM_START}assistant\nhi\n{IM_END}\n")));
        assert!(prompt.contains(&format!("{IM_START}user\nread the file\n{IM_END}\n")));
        assert!(prompt.ends_with(&format!("{IM_START}assistant\n")));
        assert_eq!(prompt.matches(IM_START).count(), 5);
        assert_eq!(prompt.matches(IM_END).count(), 4);
    }

    #[test]
    fn an_empty_tool_list_leaves_the_system_text_alone() {
        let prompt = tool_prompt("", "go", &[], &[]);
        assert!(prompt.starts_with(&format!("{IM_START}system\n\n{IM_END}\n")));
    }

    #[test]
    fn reports_a_marker_the_vocabulary_does_not_carry() {
        let vocab = Vocab::parse(br#"{"<|im_start|>":0,"a":1}"#).unwrap();
        assert_eq!(missing_markers(&vocab), vec![IM_END]);

        let vocab = Vocab::parse(br#"{"<|im_start|>":0,"<|im_end|>":1}"#).unwrap();
        assert!(missing_markers(&vocab).is_empty());
    }
}
