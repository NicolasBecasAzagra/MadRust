// SPDX-License-Identifier: GPL-2.0-only
//! The HTTP/1.1 message format of the agent's client.
//!
//! The crate carries the part of HTTP a client of an API needs today: the
//! request line and the response line, the headers with their names in lower
//! case, the body that the headers announce, and a reader that joins a message
//! which arrives in pieces. It works on bytes: bytes go in from a socket,
//! bytes come out for one, and the crate itself holds no socket and allocates
//! nothing. Each parser borrows the buffer it reads, checks a length before the
//! first read, and returns the reason of a refusal, so the same code runs in
//! the kernel and in host unit tests.
//!
//! The shapes are the ones of the wire format:
//!
//! ```text
//! GET /index.html HTTP/1.1        HTTP/1.1 200 OK
//! Host: 10.0.2.2:8080             Content-Length: 12
//! Accept: */*                     Content-Type: text/plain
//! ```
//!
//! [`RequestHead`] and [`ResponseHead`] parse the lines and the headers of a
//! message whose head is already in one piece. [`MessageReader`] is the reader
//! of a message that arrives in pieces: it collects the head, works out from
//! the headers how many body bytes to expect, and reports [`Progress`] until
//! the message is whole. [`chunked`] decodes the encoding of a body with no
//! announced length. [`write_request`] writes the request of a client, with the
//! `Host` header it has to send, and [`Url`] turns the address a person writes
//! into the parts that request needs.
//!
//! A body that is not there at all and a body of zero bytes are two different
//! messages: [`Framing::None`] is the first, [`Framing::Length`] of zero the
//! second, and a [`Response`] or a [`Request`] says which it read with
//! `body: None` or `body: Some(&[])`.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

use core::fmt;

pub mod body;
pub mod chunked;
pub mod method;
pub mod reader;
pub mod request;
pub mod response;
pub mod status;
pub mod url;

pub use body::Framing;
pub use method::Method;
pub use reader::{MessageReader, Progress};
pub use request::{write_request, Request, RequestHead};
pub use response::{Response, ResponseHead};
pub use status::StatusCode;
pub use url::Url;

/// Largest number of headers one message may carry.
///
/// The table is fixed, because the crate allocates nothing. A message with more
/// headers than this is refused with [`Error::TooManyHeaders`] instead of
/// losing one in silence.
pub const MAX_HEADERS: usize = 32;

/// The version of a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1, the version a request of this client carries.
    Http11,
}

impl Version {
    /// Parse the version token of a line.
    pub fn parse(token: &[u8]) -> Result<Self, Error> {
        match token {
            b"HTTP/1.1" => Ok(Version::Http11),
            b"HTTP/1.0" => Ok(Version::Http10),
            _ => Err(Error::UnsupportedVersion),
        }
    }

    /// The token as it is written on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Version::Http10 => "HTTP/1.0",
            Version::Http11 => "HTTP/1.1",
        }
    }
}

/// One header field: a name and a value, both borrowed from the message.
///
/// A parsed message holds the name in lower case; [`Headers::get`] compares
/// without regard to case, so a caller that builds a table by hand may write
/// the name either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header<'a> {
    /// Name of the field, a token.
    pub name: &'a [u8],
    /// Value of the field, without the spaces around it.
    pub value: &'a [u8],
}

impl<'a> Header<'a> {
    /// The placeholder of an empty slot.
    pub const EMPTY: Self = Self {
        name: &[],
        value: &[],
    };
}

/// The headers of one message, in the order they arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Headers<'a> {
    entries: [Header<'a>; MAX_HEADERS],
    len: usize,
}

impl Default for Headers<'_> {
    fn default() -> Self {
        Self {
            entries: [Header::EMPTY; MAX_HEADERS],
            len: 0,
        }
    }
}

impl<'a> Headers<'a> {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of headers in the table.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The headers in the order they arrived.
    pub fn iter(&self) -> impl Iterator<Item = &Header<'a>> {
        self.entries[..self.len].iter()
    }

    /// The value of the first header with `name`, compared without case.
    pub fn get(&self, name: &str) -> Option<&'a [u8]> {
        self.iter()
            .find(|header| header.name.eq_ignore_ascii_case(name.as_bytes()))
            .map(|header| header.value)
    }

    /// The value of `name` as text, when it is valid UTF-8.
    pub fn get_str(&self, name: &str) -> Option<&'a str> {
        self.get(name)
            .and_then(|value| core::str::from_utf8(value).ok())
    }

    /// Add a header, refusing a name that is not a token or a value with a
    /// control byte.
    pub fn push(&mut self, name: &'a [u8], value: &'a [u8]) -> Result<(), Error> {
        if name.is_empty() || !name.iter().all(|&byte| is_token_byte(byte)) {
            return Err(Error::InvalidByte {
                field: "header name",
                offset: 0,
                byte: name.first().copied().unwrap_or(0),
            });
        }
        if !is_value(value) {
            return Err(Error::ValueInvalid {
                field: "header value",
            });
        }
        if self.len == MAX_HEADERS {
            return Err(Error::TooManyHeaders { max: MAX_HEADERS });
        }
        self.entries[self.len] = Header { name, value };
        self.len += 1;
        Ok(())
    }
}

/// Reason why a message is refused.
///
/// The variants name the field that is wrong and the offset it was read at,
/// because a message that is dropped without a word cannot be told apart from
/// one that never arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The buffer is shorter than the line or the fixed part it must hold.
    TooShort { needed: usize, found: usize },
    /// The head of a message did not end in the buffer.
    HeaderTooLong { max: usize },
    /// A buffer handed to the crate cannot hold what the message carries.
    BufferFull { capacity: usize },
    /// The message carries more headers than the table holds.
    TooManyHeaders { max: usize },
    /// A line of the head is not in the form the format gives it.
    MalformedHeader { reason: &'static str },
    /// A byte does not fit the token or the value of the field.
    InvalidByte {
        field: &'static str,
        offset: usize,
        byte: u8,
    },
    /// The version of the line is neither HTTP/1.1 nor HTTP/1.0.
    UnsupportedVersion,
    /// The method of the request line is not one this crate knows.
    UnsupportedMethod,
    /// The status line is not `VERSION SP CODE [SP REASON]`.
    BadStatusLine,
    /// The status code is not three digits.
    BadStatusCode { found: usize },
    /// The target of the request line is not an origin-form path.
    TargetInvalid,
    /// A header value has a shape the field does not allow.
    ValueInvalid { field: &'static str },
    /// A response announces neither a length nor the chunked encoding.
    LengthMissing,
    /// The size line of a chunk is not hexadecimal.
    ChunkSizeInvalid { offset: usize },
    /// The bytes after a chunk are not the CRLF the encoding requires.
    ChunkDelimiter { offset: usize },
    /// A trailer line is not a header line.
    TrailerInvalid { offset: usize },
    /// More bytes arrived than the message announced.
    TrailingBytes { count: usize },
    /// The message is not complete yet.
    Incomplete,
    /// The reader was asked for the other kind of message.
    WrongKind,
    /// A request carries no `Host` header.
    HostMissing,
    /// A request carries more than one `Host` header.
    HostDuplicate,
    /// A request names a header the writer owns.
    ReservedHeader,
    /// The scheme of an address is not `http`.
    SchemeUnsupported,
    /// The address is not one this crate can take apart.
    UrlMalformed { reason: &'static str },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort { needed, found } => write!(
                formatter,
                "the buffer holds {found} bytes, the line needs {needed}"
            ),
            Error::HeaderTooLong { max } => {
                write!(formatter, "the head is not complete in {max} bytes")
            }
            Error::BufferFull { capacity } => {
                write!(formatter, "the message does not fit in {capacity} bytes")
            }
            Error::TooManyHeaders { max } => {
                write!(formatter, "the message carries more than {max} headers")
            }
            Error::MalformedHeader { reason } => write!(formatter, "{reason}"),
            Error::InvalidByte {
                field,
                offset,
                byte,
            } => write!(formatter, "{field} holds the byte 0x{byte:02x} at {offset}"),
            Error::UnsupportedVersion => {
                write!(formatter, "the version is neither HTTP/1.1 nor HTTP/1.0")
            }
            Error::UnsupportedMethod => write!(formatter, "the method is not one this crate knows"),
            Error::BadStatusLine => {
                write!(
                    formatter,
                    "the status line is not VERSION SP CODE [SP REASON]"
                )
            }
            Error::BadStatusCode { found } => {
                write!(
                    formatter,
                    "the status code is {found} bytes, not three digits"
                )
            }
            Error::TargetInvalid => {
                write!(formatter, "the target is not an absolute path")
            }
            Error::ValueInvalid { field } => write!(formatter, "{field} has an invalid value"),
            Error::LengthMissing => write!(
                formatter,
                "the response announces neither a length nor the chunked encoding"
            ),
            Error::ChunkSizeInvalid { offset } => {
                write!(
                    formatter,
                    "the chunk size at byte {offset} is not hexadecimal"
                )
            }
            Error::ChunkDelimiter { offset } => write!(
                formatter,
                "the chunk data at byte {offset} is not followed by CRLF"
            ),
            Error::TrailerInvalid { offset } => {
                write!(
                    formatter,
                    "the trailer at byte {offset} is not a header line"
                )
            }
            Error::TrailingBytes { count } => {
                write!(formatter, "{count} bytes follow the end of the message")
            }
            Error::Incomplete => write!(formatter, "the message is not complete"),
            Error::WrongKind => write!(formatter, "the reader holds the other kind of message"),
            Error::HostMissing => write!(formatter, "the request carries no Host header"),
            Error::HostDuplicate => write!(formatter, "the request carries two Host headers"),
            Error::ReservedHeader => {
                write!(formatter, "the writer owns this header, it cannot be given")
            }
            Error::SchemeUnsupported => write!(formatter, "the scheme is not http"),
            Error::UrlMalformed { reason } => write!(formatter, "the address is invalid: {reason}"),
        }
    }
}

impl core::error::Error for Error {}

/// Whether `byte` is one of the token bytes of RFC 7230.
pub(crate) fn is_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
    ) || byte.is_ascii_alphanumeric()
}

/// Whether `byte` may appear in a header value: HTAB, visible ASCII, obs-text.
pub(crate) fn is_value_byte(byte: u8) -> bool {
    byte == b'\t' || (0x20..=0x7e).contains(&byte) || byte >= 0x80
}

/// Whether every byte of `value` may appear in a header value.
pub(crate) fn is_value(value: &[u8]) -> bool {
    value.iter().all(|&byte| is_value_byte(byte))
}

/// Offset just past the empty line that ends a head, when it is in `bytes`.
pub(crate) fn find_head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

/// The line that starts at `start`: its end and the offset after its CRLF.
pub(crate) fn split_line(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let rest = bytes.get(start..)?;
    rest.windows(2)
        .position(|window| window == b"\r\n")
        .map(|length| (start + length, start + length + 2))
}

/// Remove the optional whitespace around a value, as a range.
fn trim_ows(bytes: &[u8], start: usize, end: usize) -> (usize, usize) {
    let mut first = start;
    let mut last = end;
    while first < last && (bytes[first] == b' ' || bytes[first] == b'\t') {
        first += 1;
    }
    while last > first && (bytes[last - 1] == b' ' || bytes[last - 1] == b'\t') {
        last -= 1;
    }
    (first, last)
}

/// Check the header lines of a head that starts at `start`, and lowercase the
/// names in place.
///
/// The head must end with the empty line: [`find_head_end`] has already
/// established that for the reader. This pass writes to the buffer and returns
/// nothing borrowed, so a caller may take the slices it needs afterwards with
/// [`parse_headers`].
pub(crate) fn lowercase_headers(head: &mut [u8], start: usize) -> Result<(), Error> {
    let mut count = 0;
    let mut position = start;
    while position < head.len() {
        let Some((end, next)) = split_line(head, position) else {
            return Err(Error::MalformedHeader {
                reason: "a header line does not end in CRLF",
            });
        };
        if end == position {
            if next != head.len() {
                return Err(Error::MalformedHeader {
                    reason: "bytes follow the empty line of the head",
                });
            }
            return Ok(());
        }
        if count == MAX_HEADERS {
            return Err(Error::TooManyHeaders { max: MAX_HEADERS });
        }
        let Some(colon) = head[position..end].iter().position(|&byte| byte == b':') else {
            return Err(Error::MalformedHeader {
                reason: "a header line has no colon",
            });
        };
        let colon = position + colon;
        if colon == position {
            return Err(Error::MalformedHeader {
                reason: "a header line has an empty name",
            });
        }
        for (index, byte) in head[position..colon].iter_mut().enumerate() {
            let value = *byte;
            if !is_token_byte(value) {
                return Err(Error::InvalidByte {
                    field: "header name",
                    offset: position + index,
                    byte: value,
                });
            }
            *byte = value.to_ascii_lowercase();
        }
        let (value_start, value_end) = trim_ows(head, colon + 1, end);
        for (offset, &byte) in head[value_start..value_end].iter().enumerate() {
            if !is_value_byte(byte) {
                return Err(Error::InvalidByte {
                    field: "header value",
                    offset: value_start + offset,
                    byte,
                });
            }
        }
        count += 1;
        position = next;
    }
    Err(Error::MalformedHeader {
        reason: "the head does not end with an empty line",
    })
}

/// The header table of a head that starts at `start`.
///
/// [`lowercase_headers`] must have accepted the same head first; this pass only
/// reads the bytes and borrows them.
pub(crate) fn parse_headers(head: &[u8], start: usize) -> Result<Headers<'_>, Error> {
    let mut headers = Headers::new();
    let mut position = start;
    while position < head.len() {
        let Some((end, next)) = split_line(head, position) else {
            return Err(Error::MalformedHeader {
                reason: "a header line does not end in CRLF",
            });
        };
        if end == position {
            return Ok(headers);
        }
        let Some(colon) = head[position..end].iter().position(|&byte| byte == b':') else {
            return Err(Error::MalformedHeader {
                reason: "a header line has no colon",
            });
        };
        let colon = position + colon;
        let (value_start, value_end) = trim_ows(head, colon + 1, end);
        headers.push(&head[position..colon], &head[value_start..value_end])?;
        position = next;
    }
    Err(Error::MalformedHeader {
        reason: "the head does not end with an empty line",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_version_is_one_of_the_two_the_client_speaks() {
        assert_eq!(Version::parse(b"HTTP/1.1"), Ok(Version::Http11));
        assert_eq!(Version::parse(b"HTTP/1.0"), Ok(Version::Http10));
        assert_eq!(Version::parse(b"HTTP/2"), Err(Error::UnsupportedVersion));
        assert_eq!(Version::Http11.as_str(), "HTTP/1.1");
    }

    #[test]
    fn the_head_ends_at_the_empty_line() {
        assert_eq!(find_head_end(b"A: b\r\n\r\nB: c\r\n"), Some(8));
        assert_eq!(find_head_end(b"A: b\r\n"), None);
    }

    #[test]
    fn a_header_table_compares_its_names_without_case() {
        let mut headers = Headers::new();
        headers
            .push(b"Content-Type", b"text/plain")
            .expect("the header is valid");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers.get("content-type"), Some(b"text/plain".as_slice()));
        assert_eq!(headers.get("CONTENT-TYPE"), Some(b"text/plain".as_slice()));
        assert_eq!(headers.get("server"), None);
        assert_eq!(headers.get_str("content-type"), Some("text/plain"));
    }

    #[test]
    fn a_header_with_a_control_byte_is_refused() {
        let mut headers = Headers::new();
        assert!(headers.push(b"x", b"a\rb").is_err());
        assert!(headers.push(b"x y", b"a").is_err());
        assert!(headers.push(b"", b"a").is_err());
    }

    #[test]
    fn parse_lowercases_the_names_and_trims_the_values() {
        let mut head = b"x-ONE:  a b  \r\nX-Two:\t c \r\n\r\n".to_vec();
        lowercase_headers(&mut head, 0).expect("the head is well formed");
        let headers = parse_headers(&head, 0).expect("the head is well formed");
        assert_eq!(headers.len(), 2);
        assert_eq!(
            headers.iter().next().map(|header| header.name),
            Some(b"x-one".as_slice())
        );
        assert_eq!(headers.get("x-one"), Some(b"a b".as_slice()));
        assert_eq!(headers.get("x-two"), Some(b"c".as_slice()));
    }

    #[test]
    fn a_header_line_without_a_colon_says_so() {
        let mut head = b"x-one a\r\n\r\n".to_vec();
        let error = lowercase_headers(&mut head, 0).expect_err("the line has no colon");
        assert_eq!(error.to_string(), "a header line has no colon");
    }

    #[test]
    fn the_reason_of_a_refusal_is_a_sentence() {
        assert_eq!(
            Error::TooShort {
                needed: 4,
                found: 1
            }
            .to_string(),
            "the buffer holds 1 bytes, the line needs 4"
        );
        assert_eq!(
            Error::HostMissing.to_string(),
            "the request carries no Host header"
        );
    }
}
