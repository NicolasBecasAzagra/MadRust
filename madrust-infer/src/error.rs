// SPDX-License-Identifier: GPL-2.0-only
//! One error type for the runtime.
//!
//! The runtime path never panics: a malformed model, a missing tensor or a
//! refused system call becomes an [`Error`] with a message the serial log can
//! show. The message owns its text because it can include a tensor name or an
//! error number, and it is built only on the failure path.

use core::fmt;

/// Result alias used throughout the crate.
pub type Result<T> = core::result::Result<T, Error>;

/// A runtime failure with a human readable description.
#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    /// Build an error from any displayable value.
    pub fn new(message: impl fmt::Display) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self { message }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!("i/o error: {error}"))
    }
}
