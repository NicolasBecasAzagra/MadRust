// SPDX-License-Identifier: GPL-2.0-only
//! The one optional argument of the runtime binary.
//!
//! The guest starts the program with no arguments, so the default is what the
//! initramfs run has always used. A native run can ask for more tokens, because
//! a token rate measured over eight tokens is dominated by the first few. The
//! parser takes an iterator instead of reading the process environment, so the
//! rules are tested on the host without a guest.

use std::ffi::OsString;

use crate::error::{Error, Result};

/// Tokens to generate when the command line does not say.
pub const DEFAULT_TOKENS: usize = 8;

/// Parse the optional token count from the arguments after the program name.
pub fn token_count(arguments: impl Iterator<Item = OsString>) -> Result<usize> {
    let mut arguments = arguments;
    let Some(argument) = arguments.next() else {
        return Ok(DEFAULT_TOKENS);
    };
    if arguments.next().is_some() {
        return Err(Error::new("usage: madrust-infer [TOKENS]"));
    }
    let text = argument
        .to_str()
        .ok_or_else(|| Error::new("the token count is not valid UTF-8"))?;
    let count = text
        .parse::<usize>()
        .map_err(|_| Error::new("the token count must be a positive integer"))?;
    if count == 0 {
        return Err(Error::new("the token count must be positive"));
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::{token_count, DEFAULT_TOKENS};
    use std::ffi::OsString;

    fn parse(arguments: &[&str]) -> Result<usize, crate::error::Error> {
        token_count(arguments.iter().map(OsString::from))
    }

    #[test]
    fn no_argument_keeps_the_reference_count() {
        assert_eq!(parse(&[]).unwrap(), DEFAULT_TOKENS);
        assert_eq!(DEFAULT_TOKENS, 8);
    }

    #[test]
    fn one_argument_is_the_count() {
        assert_eq!(parse(&["64"]).unwrap(), 64);
    }

    #[test]
    fn rejects_zero_extra_and_non_numeric_arguments() {
        assert!(parse(&["0"]).is_err());
        assert!(parse(&["1", "2"]).is_err());
        assert!(parse(&["many"]).is_err());
        assert!(parse(&["-1"]).is_err());
    }
}
