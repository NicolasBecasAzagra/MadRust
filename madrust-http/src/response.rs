// SPDX-License-Identifier: GPL-2.0-only
//! The status line and the headers of a response.

use crate::body::response_framing;
use crate::status::StatusCode;
use crate::{is_value_byte, parse_headers, split_line, Error, Framing, Headers, Version};

/// The head of a response: the line and the headers, without the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseHead<'a> {
    /// Version of the response.
    pub version: Version,
    /// Status of the response.
    pub status: StatusCode,
    /// The reason the server wrote, which may be empty.
    pub reason: &'a str,
    /// Headers, with their names in lower case.
    pub headers: Headers<'a>,
}

impl<'a> ResponseHead<'a> {
    /// Parse a head that ends with the empty line.
    ///
    /// The names of the headers are lowercased in place, so the buffer must be
    /// the one the message arrived in.
    pub fn parse(head: &'a mut [u8]) -> Result<Self, Error> {
        let Some((line_end, next)) = split_line(head, 0) else {
            return Err(Error::TooShort {
                needed: 2,
                found: head.len(),
            });
        };
        let line = &head[..line_end];
        let Some(first) = line.iter().position(|&byte| byte == b' ') else {
            return Err(Error::BadStatusLine);
        };
        let version = Version::parse(&line[..first])?;
        let rest_start = first + 1;
        let (code_range, reason_range) =
            match line[rest_start..].iter().position(|&byte| byte == b' ') {
                Some(offset) => (
                    rest_start..rest_start + offset,
                    rest_start + offset + 1..line_end,
                ),
                None => (rest_start..line_end, line_end..line_end),
            };
        let status = StatusCode::parse(&head[code_range]).map_err(|_| Error::BadStatusLine)?;
        if status.as_u16() < 100 {
            return Err(Error::BadStatusLine);
        }
        if !head[reason_range.clone()]
            .iter()
            .all(|&byte| is_value_byte(byte))
        {
            return Err(Error::ValueInvalid { field: "reason" });
        }
        crate::lowercase_headers(head, next)?;
        let headers = parse_headers(head, next)?;
        let reason = core::str::from_utf8(&head[reason_range]).map_err(|_| Error::BadStatusLine)?;
        Ok(Self {
            version,
            status,
            reason,
            headers,
        })
    }

    /// The reason of the response: the one the server wrote, or the standard
    /// one of the code when the line carried none.
    pub fn reason_text(&self) -> &'a str {
        if self.reason.is_empty() {
            self.status.reason()
        } else {
            self.reason
        }
    }

    /// The body the status and the headers announce.
    pub fn framing(&self) -> Result<Framing, Error> {
        response_framing(self.status, &self.headers)
    }
}

/// A response: the head and the body it announced.
///
/// `body` is `None` when the message carries no body at all, and `Some(&[])`
/// when it carries a body of zero bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response<'a> {
    /// The status line and the headers.
    pub head: ResponseHead<'a>,
    /// The body, when the message carried one.
    pub body: Option<&'a [u8]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_line_carries_the_version_the_code_and_the_reason() {
        let mut head = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n".to_vec();
        let head = ResponseHead::parse(&mut head).expect("the response is well formed");
        assert_eq!(head.version, Version::Http11);
        assert_eq!(head.status, StatusCode::OK);
        assert_eq!(head.reason, "OK");
        assert_eq!(head.reason_text(), "OK");
        assert_eq!(head.framing(), Ok(Framing::Length(2)));
    }

    #[test]
    fn a_reason_with_spaces_stays_whole() {
        let mut head = b"HTTP/1.1 404 Not Found\r\n\r\n".to_vec();
        let head = ResponseHead::parse(&mut head).expect("the response is well formed");
        assert_eq!(head.status, StatusCode::NOT_FOUND);
        assert_eq!(head.reason, "Not Found");
    }

    #[test]
    fn an_empty_reason_falls_back_to_the_standard_one() {
        let mut head = b"HTTP/1.0 204 \r\n\r\n".to_vec();
        let head = ResponseHead::parse(&mut head).expect("the response is well formed");
        assert_eq!(head.reason, "");
        assert_eq!(head.reason_text(), "No Content");
        assert_eq!(head.framing(), Ok(Framing::None));
    }

    #[test]
    fn a_status_line_that_is_not_three_parts_says_so() {
        let mut head = b"HTTP/1.1\r\n\r\n".to_vec();
        assert_eq!(ResponseHead::parse(&mut head), Err(Error::BadStatusLine));
        let mut head = b"HTTP/1.1 two OK\r\n\r\n".to_vec();
        assert_eq!(ResponseHead::parse(&mut head), Err(Error::BadStatusLine));
    }

    #[test]
    fn a_response_with_no_length_is_refused_when_the_body_is_read() {
        let mut head = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        let head = ResponseHead::parse(&mut head).expect("the head is well formed");
        assert_eq!(head.framing(), Err(Error::LengthMissing));
    }
}
