// SPDX-License-Identifier: GPL-2.0-only
//! How the headers of a message announce its body.

use crate::status::StatusCode;
use crate::{Error, Headers};

/// The body a message announces.
///
/// The three shapes are the ones a client of an API meets: no body at all, a
/// body of a known length, and the chunked encoding. The first two are not the
/// same message: `None` carries nothing, and `Length(0)` is a body of zero
/// bytes, which some APIs use on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// The message carries no body.
    None,
    /// The message carries exactly this many bytes.
    Length(usize),
    /// The message carries the chunked encoding.
    Chunked,
}

/// The framing of a request, from its headers.
pub(crate) fn request_framing(headers: &Headers<'_>) -> Result<Framing, Error> {
    if transfer_encoding_chunked(headers)? {
        return Ok(Framing::Chunked);
    }
    match content_length(headers)? {
        Some(length) => Ok(Framing::Length(length)),
        None => Ok(Framing::None),
    }
}

/// The framing of a response, from its status and its headers.
///
/// A status that carries no body wins over the headers, as HTTP/1.1 says: a
/// `204` with a `Content-Length` still has no body.
pub(crate) fn response_framing(
    status: StatusCode,
    headers: &Headers<'_>,
) -> Result<Framing, Error> {
    if status.has_no_body() {
        return Ok(Framing::None);
    }
    if transfer_encoding_chunked(headers)? {
        return Ok(Framing::Chunked);
    }
    match content_length(headers)? {
        Some(length) => Ok(Framing::Length(length)),
        None => Err(Error::LengthMissing),
    }
}

/// Whether the message announces the chunked encoding.
///
/// A transfer coding other than chunked is refused rather than guessed: this
/// client decodes chunked and nothing else.
fn transfer_encoding_chunked(headers: &Headers<'_>) -> Result<bool, Error> {
    let mut seen = false;
    for header in headers.iter() {
        if header.name.eq_ignore_ascii_case(b"transfer-encoding") {
            seen = true;
            let value = core::str::from_utf8(header.value)
                .map_err(|_| Error::ValueInvalid {
                    field: "Transfer-Encoding",
                })?
                .trim();
            if !value.eq_ignore_ascii_case("chunked") {
                return Err(Error::ValueInvalid {
                    field: "Transfer-Encoding",
                });
            }
        }
    }
    Ok(seen)
}

/// The length the `Content-Length` headers announce.
///
/// Two of them are accepted only when they carry the same number, which is what
/// RFC 7230 allows and what keeps a message from having two lengths.
fn content_length(headers: &Headers<'_>) -> Result<Option<usize>, Error> {
    let mut length: Option<usize> = None;
    for header in headers.iter() {
        if !header.name.eq_ignore_ascii_case(b"content-length") {
            continue;
        }
        let value = core::str::from_utf8(header.value)
            .map_err(|_| Error::ValueInvalid {
                field: "Content-Length",
            })?
            .trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(Error::ValueInvalid {
                field: "Content-Length",
            });
        }
        let number: usize = value.parse().map_err(|_| Error::ValueInvalid {
            field: "Content-Length",
        })?;
        match length {
            Some(previous) if previous != number => {
                return Err(Error::ValueInvalid {
                    field: "Content-Length",
                })
            }
            Some(_) => {}
            None => length = Some(number),
        }
    }
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(fields: &[(&'static str, &'static str)]) -> Headers<'static> {
        let mut table = Headers::new();
        for (name, value) in fields {
            table
                .push(name.as_bytes(), value.as_bytes())
                .expect("the test header is valid");
        }
        table
    }

    #[test]
    fn a_request_without_a_length_has_no_body() {
        let table = headers(&[("host", "h")]);
        assert_eq!(request_framing(&table), Ok(Framing::None));
    }

    #[test]
    fn a_length_of_zero_is_a_body_and_not_its_absence() {
        let table = headers(&[("content-length", "0")]);
        assert_eq!(request_framing(&table), Ok(Framing::Length(0)));
    }

    #[test]
    fn the_chunked_encoding_wins_over_a_length() {
        let table = headers(&[("content-length", "12"), ("transfer-encoding", "chunked")]);
        assert_eq!(request_framing(&table), Ok(Framing::Chunked));
    }

    #[test]
    fn a_transfer_coding_other_than_chunked_is_refused() {
        let table = headers(&[("transfer-encoding", "gzip")]);
        assert!(request_framing(&table).is_err());
    }

    #[test]
    fn two_lengths_are_accepted_only_when_they_agree() {
        let table = headers(&[("content-length", "3"), ("content-length", "3")]);
        assert_eq!(request_framing(&table), Ok(Framing::Length(3)));
        let table = headers(&[("content-length", "3"), ("content-length", "4")]);
        assert!(request_framing(&table).is_err());
        let table = headers(&[("content-length", "x")]);
        assert!(request_framing(&table).is_err());
    }

    #[test]
    fn a_response_without_a_length_or_a_chunked_body_is_an_error() {
        let table = headers(&[]);
        assert_eq!(
            response_framing(StatusCode::OK, &table),
            Err(Error::LengthMissing)
        );
        assert_eq!(
            response_framing(StatusCode::NO_CONTENT, &table),
            Ok(Framing::None)
        );
        let table = headers(&[("content-length", "2")]);
        assert_eq!(
            response_framing(StatusCode::OK, &table),
            Ok(Framing::Length(2))
        );
    }
}
