// SPDX-License-Identifier: GPL-2.0-only
//! The status code of a response line, and the reason that goes with it.

use crate::Error;

/// The status code of a response.
///
/// The code is three digits on the wire. [`StatusCode::reason`] gives the
/// standard reason of the codes a client of an API meets, and the response
/// line keeps the reason the server wrote, which is not always the same text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusCode(u16);

impl StatusCode {
    /// `100 Continue`.
    pub const CONTINUE: Self = Self(100);
    /// `101 Switching Protocols`.
    pub const SWITCHING_PROTOCOLS: Self = Self(101);
    /// `200 OK`.
    pub const OK: Self = Self(200);
    /// `201 Created`.
    pub const CREATED: Self = Self(201);
    /// `202 Accepted`.
    pub const ACCEPTED: Self = Self(202);
    /// `204 No Content`.
    pub const NO_CONTENT: Self = Self(204);
    /// `206 Partial Content`.
    pub const PARTIAL_CONTENT: Self = Self(206);
    /// `301 Moved Permanently`.
    pub const MOVED_PERMANENTLY: Self = Self(301);
    /// `302 Found`.
    pub const FOUND: Self = Self(302);
    /// `304 Not Modified`.
    pub const NOT_MODIFIED: Self = Self(304);
    /// `400 Bad Request`.
    pub const BAD_REQUEST: Self = Self(400);
    /// `401 Unauthorized`.
    pub const UNAUTHORIZED: Self = Self(401);
    /// `403 Forbidden`.
    pub const FORBIDDEN: Self = Self(403);
    /// `404 Not Found`.
    pub const NOT_FOUND: Self = Self(404);
    /// `405 Method Not Allowed`.
    pub const METHOD_NOT_ALLOWED: Self = Self(405);
    /// `408 Request Timeout`.
    pub const REQUEST_TIMEOUT: Self = Self(408);
    /// `411 Length Required`.
    pub const LENGTH_REQUIRED: Self = Self(411);
    /// `413 Payload Too Large`.
    pub const PAYLOAD_TOO_LARGE: Self = Self(413);
    /// `414 URI Too Long`.
    pub const URI_TOO_LONG: Self = Self(414);
    /// `431 Request Header Fields Too Large`.
    pub const HEADER_FIELDS_TOO_LARGE: Self = Self(431);
    /// `500 Internal Server Error`.
    pub const INTERNAL_SERVER_ERROR: Self = Self(500);
    /// `501 Not Implemented`.
    pub const NOT_IMPLEMENTED: Self = Self(501);
    /// `502 Bad Gateway`.
    pub const BAD_GATEWAY: Self = Self(502);
    /// `503 Service Unavailable`.
    pub const SERVICE_UNAVAILABLE: Self = Self(503);
    /// `505 HTTP Version Not Supported`.
    pub const HTTP_VERSION_NOT_SUPPORTED: Self = Self(505);

    /// The code a server writes, or the number it was built from.
    pub const fn from_u16(code: u16) -> Self {
        Self(code)
    }

    /// The number of the code.
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// Parse the three digits of a response line.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 3 {
            return Err(Error::BadStatusCode { found: bytes.len() });
        }
        let mut code = 0u16;
        for &byte in bytes {
            if !byte.is_ascii_digit() {
                return Err(Error::BadStatusCode { found: bytes.len() });
            }
            code = code * 10 + u16::from(byte - b'0');
        }
        Ok(Self(code))
    }

    /// The standard reason of the code, or `"Unknown"` for a code this crate
    /// does not name.
    pub fn reason(self) -> &'static str {
        match self.0 {
            100 => "Continue",
            101 => "Switching Protocols",
            200 => "OK",
            201 => "Created",
            202 => "Accepted",
            204 => "No Content",
            206 => "Partial Content",
            301 => "Moved Permanently",
            302 => "Found",
            304 => "Not Modified",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            408 => "Request Timeout",
            411 => "Length Required",
            413 => "Payload Too Large",
            414 => "URI Too Long",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            505 => "HTTP Version Not Supported",
            _ => "Unknown",
        }
    }

    /// Whether the code is an informational one.
    pub fn is_informational(self) -> bool {
        (100..200).contains(&self.0)
    }

    /// Whether the code is a success.
    pub fn is_success(self) -> bool {
        (200..300).contains(&self.0)
    }

    /// Whether a response with this code carries no body, whatever its headers
    /// say.
    ///
    /// The informational codes, `204` and `304` are the ones of HTTP/1.1. A
    /// response to `HEAD` has no body too, which the request knows and the code
    /// does not.
    pub fn has_no_body(self) -> bool {
        self.is_informational() || self.0 == 204 || self.0 == 304
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_three_digits() {
        assert_eq!(StatusCode::parse(b"200"), Ok(StatusCode::OK));
        assert_eq!(StatusCode::parse(b"404"), Ok(StatusCode::NOT_FOUND));
        assert_eq!(
            StatusCode::parse(b"20"),
            Err(Error::BadStatusCode { found: 2 })
        );
        assert_eq!(
            StatusCode::parse(b"2x0"),
            Err(Error::BadStatusCode { found: 3 })
        );
    }

    #[test]
    fn a_code_carries_its_reason() {
        assert_eq!(StatusCode::OK.reason(), "OK");
        assert_eq!(StatusCode::NOT_FOUND.reason(), "Not Found");
        assert_eq!(StatusCode::from_u16(299).reason(), "Unknown");
        assert!(StatusCode::OK.is_success());
        assert!(!StatusCode::NOT_FOUND.is_success());
        assert!(StatusCode::NO_CONTENT.has_no_body());
        assert!(StatusCode::CONTINUE.has_no_body());
        assert!(!StatusCode::OK.has_no_body());
    }
}
