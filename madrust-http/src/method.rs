// SPDX-License-Identifier: GPL-2.0-only
//! The method of a request line.

use crate::Error;

/// The method of a request.
///
/// The list is the one of HTTP/1.1. A method outside it is refused, because a
/// client of an API sends a method it chose and a server that reads another one
/// would take it for a mistake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`: read a resource.
    Get,
    /// `HEAD`: read the head of a resource.
    Head,
    /// `POST`: send a body to a resource.
    Post,
    /// `PUT`: replace a resource.
    Put,
    /// `DELETE`: remove a resource.
    Delete,
    /// `CONNECT`: open a tunnel.
    Connect,
    /// `OPTIONS`: ask what a resource allows.
    Options,
    /// `TRACE`: echo the request.
    Trace,
    /// `PATCH`: change part of a resource.
    Patch,
}

impl Method {
    /// Parse the method token of a request line.
    pub fn parse(token: &[u8]) -> Result<Self, Error> {
        match token {
            b"GET" => Ok(Method::Get),
            b"HEAD" => Ok(Method::Head),
            b"POST" => Ok(Method::Post),
            b"PUT" => Ok(Method::Put),
            b"DELETE" => Ok(Method::Delete),
            b"CONNECT" => Ok(Method::Connect),
            b"OPTIONS" => Ok(Method::Options),
            b"TRACE" => Ok(Method::Trace),
            b"PATCH" => Ok(Method::Patch),
            _ => Err(Error::UnsupportedMethod),
        }
    }

    /// The token as it is written on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Connect => "CONNECT",
            Method::Options => "OPTIONS",
            Method::Trace => "TRACE",
            Method::Patch => "PATCH",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_round_trips_through_its_token() {
        for method in [
            Method::Get,
            Method::Head,
            Method::Post,
            Method::Put,
            Method::Delete,
            Method::Connect,
            Method::Options,
            Method::Trace,
            Method::Patch,
        ] {
            assert_eq!(Method::parse(method.as_str().as_bytes()), Ok(method));
        }
        assert_eq!(Method::parse(b"BREW"), Err(Error::UnsupportedMethod));
    }
}
