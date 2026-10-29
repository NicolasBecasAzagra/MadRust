// SPDX-License-Identifier: GPL-2.0-only
//! The reader of a message that arrives in pieces.
//!
//! A socket hands over what the network brought, which is not a message: the
//! bytes can stop in the middle of a header, in the middle of the body, or in
//! the middle of the CRLF that ends a line. [`MessageReader`] collects them,
//! parses the head as soon as the empty line is in hand, and uses the headers to
//! learn how much body belongs to the message. [`MessageReader::feed`] says
//! [`Progress::NeedMore`] until the message is whole, and the accessor then
//! gives the head and the body.
//!
//! The reader is handed two buffers and allocates nothing: `input` holds the
//! bytes exactly as they arrived, and `body` receives the body without the
//! chunked encoding, which needs a second region because a decoded body does
//! not fit where the encoding is without a copy. A message larger than either
//! buffer is refused with [`Error::BufferFull`], never cut.

use crate::chunked;
use crate::request::RequestHead;
use crate::response::ResponseHead;
use crate::{Error, Framing, Request, Response};

/// Whether the reader collects a request or a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The bytes are a request, the message a server reads.
    Request,
    /// The bytes are a response, the message a client reads.
    Response,
}

/// How far the message has come.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The bytes so far do not hold the whole message.
    NeedMore,
    /// The message is whole, and the accessor gives it.
    Complete,
}

/// The reader of one message.
pub struct MessageReader<'buf> {
    mode: Mode,
    input: &'buf mut [u8],
    body: &'buf mut [u8],
    filled: usize,
    head_len: Option<usize>,
    framing: Framing,
    body_len: Option<usize>,
    complete: bool,
}

impl<'buf> MessageReader<'buf> {
    /// A reader of a request.
    pub fn for_request(input: &'buf mut [u8], body: &'buf mut [u8]) -> Self {
        Self::new(Mode::Request, input, body)
    }

    /// A reader of a response.
    pub fn for_response(input: &'buf mut [u8], body: &'buf mut [u8]) -> Self {
        Self::new(Mode::Response, input, body)
    }

    fn new(mode: Mode, input: &'buf mut [u8], body: &'buf mut [u8]) -> Self {
        Self {
            mode,
            input,
            body,
            filled: 0,
            head_len: None,
            framing: Framing::None,
            body_len: None,
            complete: false,
        }
    }

    /// Add the bytes a socket handed over and report how far the message is.
    ///
    /// A message that is already whole refuses further bytes with
    /// [`Error::TrailingBytes`]: the caller has one message per reader, and a
    /// byte after it belongs to a message this reader does not hold.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Progress, Error> {
        if self.complete {
            if bytes.is_empty() {
                return Ok(Progress::Complete);
            }
            return Err(Error::TrailingBytes { count: bytes.len() });
        }
        let end = self
            .filled
            .checked_add(bytes.len())
            .ok_or(Error::BufferFull {
                capacity: self.input.len(),
            })?;
        if end > self.input.len() {
            return Err(Error::BufferFull {
                capacity: self.input.len(),
            });
        }
        self.input[self.filled..end].copy_from_slice(bytes);
        self.filled = end;

        if self.head_len.is_none() {
            let Some(head_end) = crate::find_head_end(&self.input[..self.filled]) else {
                return Ok(Progress::NeedMore);
            };
            let framing = match self.mode {
                Mode::Request => RequestHead::parse(&mut self.input[..head_end])?.framing()?,
                Mode::Response => ResponseHead::parse(&mut self.input[..head_end])?.framing()?,
            };
            self.head_len = Some(head_end);
            self.framing = framing;
            if framing == Framing::None {
                self.body_len = None;
                self.complete = true;
                return Ok(Progress::Complete);
            }
        }

        let head_len = self.head_len.unwrap_or(0);
        let found = self.filled - head_len;
        match self.framing {
            Framing::None => {
                self.body_len = None;
                self.complete = true;
                Ok(Progress::Complete)
            }
            Framing::Length(length) => {
                if found > length {
                    return Err(Error::TrailingBytes {
                        count: found - length,
                    });
                }
                if found < length {
                    return Ok(Progress::NeedMore);
                }
                self.copy_body(head_len, length)?;
                self.body_len = Some(length);
                self.complete = true;
                Ok(Progress::Complete)
            }
            Framing::Chunked => {
                let encoded = &self.input[head_len..self.filled];
                let Some((consumed, produced)) = chunked::decode(encoded, self.body)? else {
                    return Ok(Progress::NeedMore);
                };
                if consumed != encoded.len() {
                    return Err(Error::TrailingBytes {
                        count: encoded.len() - consumed,
                    });
                }
                self.body_len = Some(produced);
                self.complete = true;
                Ok(Progress::Complete)
            }
        }
    }

    /// Copy a body of a known length into its own buffer.
    fn copy_body(&mut self, head_len: usize, length: usize) -> Result<(), Error> {
        if length > self.body.len() {
            return Err(Error::BufferFull {
                capacity: self.body.len(),
            });
        }
        let source = &self.input[head_len..head_len + length];
        self.body[..length].copy_from_slice(source);
        Ok(())
    }

    /// The request, once [`Progress::Complete`] said it is whole.
    pub fn request(&mut self) -> Result<Request<'_>, Error> {
        if self.mode != Mode::Request {
            return Err(Error::WrongKind);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        let Self {
            input,
            body,
            head_len,
            body_len,
            ..
        } = self;
        let head_len = (*head_len).ok_or(Error::Incomplete)?;
        let head = RequestHead::parse(&mut input[..head_len])?;
        let body = body_len.map(|length| &body[..length]);
        Ok(Request { head, body })
    }

    /// The response, once [`Progress::Complete`] said it is whole.
    pub fn response(&mut self) -> Result<Response<'_>, Error> {
        if self.mode != Mode::Response {
            return Err(Error::WrongKind);
        }
        if !self.complete {
            return Err(Error::Incomplete);
        }
        let Self {
            input,
            body,
            head_len,
            body_len,
            ..
        } = self;
        let head_len = (*head_len).ok_or(Error::Incomplete)?;
        let head = ResponseHead::parse(&mut input[..head_len])?;
        let body = body_len.map(|length| &body[..length]);
        Ok(Response { head, body })
    }

    /// Number of bytes of the message that arrived so far.
    pub fn filled(&self) -> usize {
        self.filled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Method, StatusCode, Version};

    const RESPONSE: &[u8] =
        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/plain\r\n\r\nhello";

    fn read_response(bytes: &[u8]) -> (StatusCode, Version, Framing, Option<Vec<u8>>) {
        // The buffers outlive the reader by leaking them: a test keeps the
        // message alive long enough to inspect it.
        let input: &'static mut [u8] = Box::leak(vec![0u8; 1024].into_boxed_slice());
        let body: &'static mut [u8] = Box::leak(vec![0u8; 1024].into_boxed_slice());
        let mut reader = MessageReader::for_response(input, body);
        for byte in bytes {
            let _ = reader.feed(&[*byte]);
        }
        let response = reader.response().expect("the message is complete");
        (
            response.head.status,
            response.head.version,
            response
                .head
                .framing()
                .expect("the message frames its body"),
            response.body.map(|body| body.to_vec()),
        )
    }

    #[test]
    fn a_response_that_arrives_one_byte_at_a_time_is_joined() {
        let (status, version, _, body) = read_response(RESPONSE);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(version, Version::Http11);
        assert_eq!(body.as_deref(), Some(b"hello".as_slice()));
    }

    #[test]
    fn the_reader_reports_need_more_in_the_middle_of_a_header() {
        let input = Box::leak(vec![0u8; 256].into_boxed_slice());
        let body = Box::leak(vec![0u8; 256].into_boxed_slice());
        let mut reader = MessageReader::for_response(input, body);
        let (first, second) = RESPONSE.split_at(20);
        assert_eq!(reader.feed(first), Ok(Progress::NeedMore));
        assert_eq!(reader.response(), Err(Error::Incomplete));
        assert_eq!(reader.feed(second), Ok(Progress::Complete));
        let response = reader.response().expect("the message is complete");
        assert_eq!(response.body, Some(b"hello".as_slice()));
    }

    #[test]
    fn a_head_that_never_ends_is_refused_when_the_buffer_is_full() {
        let input = Box::leak(vec![0u8; 16].into_boxed_slice());
        let body = Box::leak(vec![0u8; 16].into_boxed_slice());
        let mut reader = MessageReader::for_response(input, body);
        assert_eq!(
            reader.feed(b"HTTP/1.1 200 OK\r\nX"),
            Err(Error::BufferFull { capacity: 16 })
        );
    }

    #[test]
    fn bytes_after_the_message_are_refused_and_named() {
        let input = Box::leak(vec![0u8; 256].into_boxed_slice());
        let body = Box::leak(vec![0u8; 256].into_boxed_slice());
        let mut reader = MessageReader::for_response(input, body);
        let mut message = RESPONSE.to_vec();
        message.extend_from_slice(b"more");
        assert_eq!(
            reader.feed(&message),
            Err(Error::TrailingBytes { count: 4 })
        );
    }

    #[test]
    fn a_body_of_zero_bytes_is_not_the_absence_of_a_body() {
        let (_, _, framing, body) = read_response(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(framing, Framing::Length(0));
        assert_eq!(body.as_deref(), Some(b"".as_slice()));

        let (_, _, framing, body) = read_response(b"HTTP/1.1 204 No Content\r\n\r\n");
        assert_eq!(framing, Framing::None);
        assert_eq!(body, None);
    }

    #[test]
    fn a_chunked_body_is_decoded_while_it_arrives() {
        let message =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let (_, _, framing, body) = read_response(message);
        assert_eq!(framing, Framing::Chunked);
        assert_eq!(body.as_deref(), Some(b"Wikipedia".as_slice()));
    }

    #[test]
    fn a_chunked_body_that_stops_inside_the_encoding_needs_more() {
        let input = Box::leak(vec![0u8; 256].into_boxed_slice());
        let body = Box::leak(vec![0u8; 256].into_boxed_slice());
        let mut reader = MessageReader::for_response(input, body);
        let message = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWi";
        assert_eq!(reader.feed(message), Ok(Progress::NeedMore));
        assert_eq!(reader.feed(b"ki\r\n0\r\n\r\n"), Ok(Progress::Complete));
        let response = reader.response().expect("the message is complete");
        assert_eq!(response.body, Some(b"Wiki".as_slice()));
    }

    #[test]
    fn a_broken_head_says_what_was_wrong() {
        let input = Box::leak(vec![0u8; 256].into_boxed_slice());
        let body = Box::leak(vec![0u8; 256].into_boxed_slice());
        let mut reader = MessageReader::for_response(input, body);
        let error = reader
            .feed(b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n")
            .expect_err("the header line has no colon");
        assert_eq!(error.to_string(), "a header line has no colon");
    }

    #[test]
    fn a_request_is_read_the_same_way() {
        let input = Box::leak(vec![0u8; 256].into_boxed_slice());
        let body = Box::leak(vec![0u8; 256].into_boxed_slice());
        let mut reader = MessageReader::for_request(input, body);
        let message = b"GET /index.html HTTP/1.1\r\nHost: 10.0.2.2:8080\r\nAccept: */*\r\n\r\n";
        assert_eq!(reader.feed(message), Ok(Progress::Complete));
        let request = reader.request().expect("the message is complete");
        assert_eq!(request.head.headers.get("accept"), Some(b"*/*".as_slice()));
        assert_eq!(request.head.method, Method::Get);
        assert_eq!(request.head.target, "/index.html");
        assert_eq!(request.head.require_host(), Ok("10.0.2.2:8080"));
    }

    #[test]
    fn the_wrong_kind_of_message_is_named() {
        let input = Box::leak(vec![0u8; 256].into_boxed_slice());
        let body = Box::leak(vec![0u8; 256].into_boxed_slice());
        let mut reader = MessageReader::for_request(input, body);
        assert_eq!(reader.response().err(), Some(Error::WrongKind));
    }
}
