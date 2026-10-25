// SPDX-License-Identifier: GPL-2.0-only
//! The published reference run of SmolLM2-135M.
//!
//! `bench/reference/reference.json` records the token ids the reference
//! implementation produces for the prompt `The capital of France is`, in
//! float32 and with greedy sampling. The runtime uses the prompt when the
//! model config does not carry one, and it compares the tokens it generates
//! against these values when the checkpoint has the dimensions of the reference
//! model. A change to the arithmetic that alters a token therefore fails the
//! run loudly instead of silently changing the text.
//!
//! A model config can still carry `expected_tokens` of its own; that value
//! wins, and the synthetic development model uses its own prompt and has no
//! golden tokens.

/// Prompt ids of `The capital of France is`, from the SmolLM2 tokenizer.
pub const PROMPT_IDS: [u32; 5] = [504, 3575, 282, 4649, 314];

/// Tokens the reference implementation generates after the prompt.
pub const NEW_TOKENS: [u32; 8] = [260, 3575, 282, 260, 1798, 30, 198, 198];

/// Prompt followed by the generated tokens, as one list.
pub const GENERATED_IDS: [u32; 13] = [
    504, 3575, 282, 4649, 314, 260, 3575, 282, 260, 1798, 30, 198, 198,
];

/// The published reference file, compiled in so a test can check the constants
/// against it without reading the file system.
#[cfg(test)]
const PUBLISHED: &str = include_str!("../../bench/reference/reference.json");

#[cfg(test)]
mod tests {
    use super::{GENERATED_IDS, NEW_TOKENS, PROMPT_IDS, PUBLISHED};

    fn ids(values: &[u32]) -> String {
        values
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[test]
    fn the_constants_are_the_published_reference() {
        assert!(
            PUBLISHED.contains(&format!("\"prompt_ids\": [{}]", ids(&PROMPT_IDS))),
            "the prompt ids differ from bench/reference/reference.json"
        );
        assert!(
            PUBLISHED.contains(&format!("\"generated_ids\": [{}]", ids(&GENERATED_IDS))),
            "the generated ids differ from bench/reference/reference.json"
        );
        assert!(
            PUBLISHED.contains(&format!("\"new_tokens\": [{}]", ids(&NEW_TOKENS))),
            "the new tokens differ from bench/reference/reference.json"
        );
    }

    #[test]
    fn the_generated_list_is_the_prompt_followed_by_the_new_tokens() {
        let mut joined = PROMPT_IDS.to_vec();
        joined.extend_from_slice(&NEW_TOKENS);
        assert_eq!(joined, GENERATED_IDS);
    }
}
