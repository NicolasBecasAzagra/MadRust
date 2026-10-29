// SPDX-License-Identifier: GPL-2.0-only
//! The request line and the headers of a request, and the writer of a request.

use crate::body::request_framing;
use crate::method::Method;
use crate::{
    is_token_byte, is_value, is_value_byte, parse_headers, split_line, Error, Framing, Headers,
    Version,
};

/// The head of a request: the line and the headers, without the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestHead<'a> {
    /// Method of the request.
    pub method: Method,
    /// Target of the request, the path and the query of the resource.
    pub target: &'a str,
    /// Version of the request.
    pub version: Version,
    /// Headers, with their names in lower case.
    pub headers: Headers<'a>,
}

impl<'a> RequestHead<'a> {
    /// Parse a head that ends with the empty line.
    ///
    /// The names of the headers are lowercased in place, so the buffer must be
    /// the one the message arrived in and not a copy the caller keeps.
    pub fn parse(head: &'a mut [u8]) -> Result<Self, Error> {
        let Some((line_end, next)) = split_line(head, 0) else {
            return Err(Error::TooShort {
                needed: 2,
                found: head.len(),
            });
        };
        let line = &head[..line_end];
        let Some(first) = line.iter().position(|&byte| byte == b' ') else {
            return Err(Error::MalformedHeader {
                reason: "the request line has no target",
            });
        };
        let Some(offset) = line[first + 1..].iter().position(|&byte| byte == b' ') else {
            return Err(Error::MalformedHeader {
                reason: "the request line has no version",
            });
        };
        let second = first + 1 + offset;
        if line[second + 1..].contains(&b' ') {
            return Err(Error::MalformedHeader {
                reason: "the request line carries more than three parts",
            });
        }
        let method = Method::parse(&line[..first])?;
        let version = Version::parse(&line[second + 1..])?;
        let target_range = first + 1..second;
        crate::lowercase_headers(head, next)?;
        let headers = parse_headers(head, next)?;
        let target = parse_target(&head[target_range])?;
        if headers
            .iter()
            .filter(|header| header.name.eq_ignore_ascii_case(b"host"))
            .count()
            > 1
        {
            return Err(Error::HostDuplicate);
        }
        Ok(Self {
            method,
            target,
            version,
            headers,
        })
    }

    /// The `Host` header, when the request carries one.
    pub fn host(&self) -> Option<&'a str> {
        self.headers.get_str("host")
    }

    /// The `Host` header, or the reason the request is refused.
    ///
    /// A server checks this before it reads the body: an HTTP/1.1 request
    /// without a host names no resource.
    pub fn require_host(&self) -> Result<&'a str, Error> {
        let Some(host) = self.host() else {
            return Err(Error::HostMissing);
        };
        if host.is_empty()
            || host
                .bytes()
                .any(|byte| !is_value_byte(byte) || byte == b' ')
        {
            return Err(Error::ValueInvalid { field: "Host" });
        }
        Ok(host)
    }

    /// The body the headers announce.
    pub fn framing(&self) -> Result<Framing, Error> {
        request_framing(&self.headers)
    }
}

/// A request: the head and the body it announced.
///
/// `body` is `None` when the request carries no body at all, and `Some(&[])`
/// when it carries a body of zero bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    /// The request line and the headers.
    pub head: RequestHead<'a>,
    /// The body, when the message carried one.
    pub body: Option<&'a [u8]>,
}

/// The target of a request line: an absolute path with an optional query.
fn parse_target(bytes: &[u8]) -> Result<&str, Error> {
    if bytes.first() != Some(&b'/') {
        return Err(Error::TargetInvalid);
    }
    if bytes.iter().any(|&byte| !(0x21..=0x7e).contains(&byte)) {
        return Err(Error::TargetInvalid);
    }
    core::str::from_utf8(bytes).map_err(|_| Error::TargetInvalid)
}

/// A buffer that is written from the front and refuses to grow.
struct Sink<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Sink<'_> {
    fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self.len.checked_add(bytes.len()).ok_or(Error::BufferFull {
            capacity: self.out.len(),
        })?;
        if end > self.out.len() {
            return Err(Error::BufferFull {
                capacity: self.out.len(),
            });
        }
        self.out[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }

    fn decimal(&mut self, mut value: usize) -> Result<(), Error> {
        let mut digits = [0u8; 20];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (value % 10) as u8;
            value /= 10;
            count += 1;
            if value == 0 {
                break;
            }
        }
        for index in (0..count).rev() {
            self.push(&[digits[index]])?;
        }
        Ok(())
    }
}

/// Whether the writer owns `name` and a caller may not give it.
fn is_reserved(name: &str) -> bool {
    ["host", "content-length", "transfer-encoding"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Write a request into `out`, and return how many bytes it used.
///
/// The writer owns the request line and the headers that frame the body:
/// `Host` comes from `host`, and `Content-Length` is written exactly when
/// `body` is `Some`, which makes "no body" and "a body of zero bytes" two
/// different messages on the wire. A caller cannot pass those headers itself,
/// so a request never carries two of them.
pub fn write_request(
    out: &mut [u8],
    method: Method,
    target: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<usize, Error> {
    if !target.starts_with('/') || !target.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(Error::TargetInvalid);
    }
    if host.is_empty()
        || host
            .bytes()
            .any(|byte| !is_value_byte(byte) || byte == b' ' || byte == b'\t')
    {
        return Err(Error::ValueInvalid { field: "Host" });
    }
    let mut sink = Sink { out, len: 0 };
    sink.push(method.as_str().as_bytes())?;
    sink.push(b" ")?;
    sink.push(target.as_bytes())?;
    sink.push(b" HTTP/1.1\r\nHost: ")?;
    sink.push(host.as_bytes())?;
    sink.push(b"\r\n")?;
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(is_token_byte) {
            return Err(Error::InvalidByte {
                field: "header name",
                offset: 0,
                byte: name.bytes().next().unwrap_or(0),
            });
        }
        if is_reserved(name) {
            return Err(Error::ReservedHeader);
        }
        if !is_value(value.as_bytes()) {
            return Err(Error::ValueInvalid {
                field: "header value",
            });
        }
        sink.push(name.as_bytes())?;
        sink.push(b": ")?;
        sink.push(value.as_bytes())?;
        sink.push(b"\r\n")?;
    }
    if let Some(body) = body {
        sink.push(b"Content-Length: ")?;
        sink.decimal(body.len())?;
        sink.push(b"\r\n")?;
    }
    sink.push(b"\r\n")?;
    if let Some(body) = body {
        sink.push(body)?;
    }
    Ok(sink.len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_request(head: &mut [u8]) -> Result<RequestHead<'_>, Error> {
        RequestHead::parse(head)
    }

    #[test]
    fn the_request_line_carries_the_method_target_and_version() {
        let mut head = b"GET /index.html?q=1 HTTP/1.1\r\nHost: 10.0.2.2\r\n\r\n".to_vec();
        let head = parse_request(&mut head).expect("the request is well formed");
        assert_eq!(head.method, Method::Get);
        assert_eq!(head.target, "/index.html?q=1");
        assert_eq!(head.version, Version::Http11);
        assert_eq!(head.host(), Some("10.0.2.2"));
        assert_eq!(head.require_host(), Ok("10.0.2.2"));
        assert_eq!(head.framing(), Ok(Framing::None));
    }

    #[test]
    fn a_request_line_of_two_parts_is_refused_with_its_reason() {
        let mut head = b"GET /index.html\r\n\r\n".to_vec();
        let error = parse_request(&mut head).expect_err("the version is missing");
        assert_eq!(error.to_string(), "the request line has no version");
    }

    #[test]
    fn a_target_that_is_not_a_path_is_refused() {
        let mut head = b"GET index.html HTTP/1.1\r\nHost: h\r\n\r\n".to_vec();
        assert_eq!(parse_request(&mut head), Err(Error::TargetInvalid));
    }

    #[test]
    fn a_request_without_a_host_is_refused_by_the_server_check() {
        let mut head = b"GET / HTTP/1.1\r\nAccept: */*\r\n\r\n".to_vec();
        let head = parse_request(&mut head).expect("the head is well formed");
        assert_eq!(head.require_host(), Err(Error::HostMissing));
    }

    #[test]
    fn two_host_headers_are_refused_at_the_parse() {
        let mut head = b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n".to_vec();
        assert_eq!(parse_request(&mut head), Err(Error::HostDuplicate));
    }

    #[test]
    fn the_writer_puts_the_host_and_the_length_where_they_belong() {
        let mut out = [0u8; 128];
        let written = write_request(
            &mut out,
            Method::Get,
            "/index.html",
            "10.0.2.2:8080",
            &[("accept", "*/*")],
            None,
        )
        .expect("the request fits");
        assert_eq!(
            &out[..written],
            b"GET /index.html HTTP/1.1\r\nHost: 10.0.2.2:8080\r\naccept: */*\r\n\r\n"
        );
    }

    #[test]
    fn a_body_of_zero_bytes_is_a_length_header_and_no_bytes() {
        let mut out = [0u8; 128];
        let written = write_request(&mut out, Method::Post, "/a", "h", &[], Some(b""))
            .expect("the request fits");
        assert_eq!(
            &out[..written],
            b"POST /a HTTP/1.1\r\nHost: h\r\nContent-Length: 0\r\n\r\n"
        );
        let written = write_request(&mut out, Method::Post, "/a", "h", &[], Some(b"xy"))
            .expect("the request fits");
        assert!(out[..written].ends_with(b"Content-Length: 2\r\n\r\nxy"));
    }

    #[test]
    fn the_writer_refuses_a_header_it_owns() {
        let mut out = [0u8; 128];
        assert_eq!(
            write_request(
                &mut out,
                Method::Get,
                "/",
                "h",
                &[("Content-Length", "1")],
                None
            ),
            Err(Error::ReservedHeader)
        );
    }

    #[test]
    fn a_request_that_does_not_fit_is_refused() {
        let mut out = [0u8; 8];
        assert_eq!(
            write_request(&mut out, Method::Get, "/index.html", "h", &[], None),
            Err(Error::BufferFull { capacity: 8 })
        );
    }
}
