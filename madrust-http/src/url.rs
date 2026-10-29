// SPDX-License-Identifier: GPL-2.0-only
//! The address of a resource, as a person writes it.
//!
//! `http://10.0.2.2:8080/index.html?q=1` is the form an argument of a tool
//! takes. [`Url`] takes it apart into the scheme, the host, the port, the path
//! and the query, because that is what the request line and the `Host` header
//! need, and it refuses an address this client cannot reach. `https` is refused
//! with [`Error::SchemeUnsupported`]: the client speaks HTTP and has no TLS.

use crate::Error;

/// An HTTP address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Url<'a> {
    /// Scheme of the address, always `http`.
    pub scheme: &'a str,
    /// Host name or address, with the brackets of an IPv6 address.
    pub host: &'a str,
    /// Port, or 80 when the address did not name one.
    pub port: u16,
    /// Path of the resource, always starting with `/`.
    pub path: &'a str,
    /// Query of the address, with its leading `?`, or empty.
    pub query: &'a str,
}

impl<'a> Url<'a> {
    /// Take an address apart.
    pub fn parse(text: &'a str) -> Result<Self, Error> {
        let text = match text.find('#') {
            Some(index) => &text[..index],
            None => text,
        };
        let Some((scheme, rest)) = text.split_once("://") else {
            return Err(Error::UrlMalformed {
                reason: "it has no scheme",
            });
        };
        if !scheme.eq_ignore_ascii_case("http") {
            return Err(Error::SchemeUnsupported);
        }
        let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let tail = &rest[authority_end..];
        if authority.is_empty() {
            return Err(Error::UrlMalformed {
                reason: "it has no host",
            });
        }
        if authority.contains('@') {
            return Err(Error::UrlMalformed {
                reason: "it carries user information",
            });
        }
        let (host, port) = split_authority(authority)?;
        if host.is_empty() {
            return Err(Error::UrlMalformed {
                reason: "it has no host",
            });
        }
        let (path, query) = match tail.find('?') {
            Some(index) => (&tail[..index], &tail[index..]),
            None => (tail, ""),
        };
        let path = if path.is_empty() { "/" } else { path };
        if !path.starts_with('/') {
            return Err(Error::UrlMalformed {
                reason: "the path does not start with a slash",
            });
        }
        if path
            .bytes()
            .chain(query.bytes())
            .any(|byte| !(0x21..=0x7e).contains(&byte))
        {
            return Err(Error::UrlMalformed {
                reason: "the path or the query holds a byte a request cannot carry",
            });
        }
        Ok(Self {
            scheme: &text[..scheme.len()],
            host,
            port,
            path,
            query,
        })
    }

    /// The target of the request line: the path and the query.
    ///
    /// A caller that needs one string writes both parts next to each other,
    /// because a slice of the address cannot hold the `/` of a path the person
    /// left out.
    pub fn target_parts(&self) -> (&'a str, &'a str) {
        (self.path, self.query)
    }
}

/// Split `host:port` of an authority, with the brackets of IPv6.
fn split_authority(authority: &str) -> Result<(&str, u16), Error> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(close) = rest.find(']') else {
            return Err(Error::UrlMalformed {
                reason: "an IPv6 address has no closing bracket",
            });
        };
        let host = &authority[..close + 2];
        let after = &authority[close + 2..];
        let port = match after.strip_prefix(':') {
            Some(port) => parse_port(port)?,
            None if after.is_empty() => 80,
            None => {
                return Err(Error::UrlMalformed {
                    reason: "the bytes after the host are not a port",
                })
            }
        };
        return Ok((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') {
                return Err(Error::UrlMalformed {
                    reason: "an IPv6 address needs brackets",
                });
            }
            Ok((host, parse_port(port)?))
        }
        None => Ok((authority, 80)),
    }
}

/// Parse the port of an authority.
fn parse_port(text: &str) -> Result<u16, Error> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Error::UrlMalformed {
            reason: "the port is not a number",
        });
    }
    let port: u16 = text.parse().map_err(|_| Error::UrlMalformed {
        reason: "the port does not fit sixteen bits",
    })?;
    if port == 0 {
        return Err(Error::UrlMalformed {
            reason: "the port is zero",
        });
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_with_a_port_and_a_query_is_taken_apart() {
        let url = Url::parse("http://10.0.2.2:8080/index.html?q=1").expect("the address is valid");
        assert_eq!(url.scheme, "http");
        assert_eq!(url.host, "10.0.2.2");
        assert_eq!(url.port, 8080);
        assert_eq!(url.path, "/index.html");
        assert_eq!(url.query, "?q=1");
    }

    #[test]
    fn the_port_defaults_to_eighty_and_the_path_to_the_root() {
        let url = Url::parse("http://example.test").expect("the address is valid");
        assert_eq!(url.port, 80);
        assert_eq!(url.path, "/");
        assert_eq!(url.query, "");
    }

    #[test]
    fn a_query_without_a_path_still_names_the_root() {
        let url = Url::parse("http://h?x=1").expect("the address is valid");
        assert_eq!(url.path, "/");
        assert_eq!(url.query, "?x=1");
    }

    #[test]
    fn a_fragment_is_not_sent() {
        let url = Url::parse("http://h/a#part").expect("the address is valid");
        assert_eq!(url.path, "/a");
        assert_eq!(url.query, "");
    }

    #[test]
    fn an_ipv6_literal_keeps_its_brackets() {
        let url = Url::parse("http://[::1]:8080/").expect("the address is valid");
        assert_eq!(url.host, "[::1]");
        assert_eq!(url.port, 8080);
        let url = Url::parse("http://[::1]/").expect("the address is valid");
        assert_eq!(url.port, 80);
    }

    #[test]
    fn an_address_the_client_cannot_reach_is_refused_with_its_reason() {
        assert_eq!(Url::parse("https://h/"), Err(Error::SchemeUnsupported));
        assert!(Url::parse("10.0.2.2:8080/x").is_err());
        assert!(Url::parse("http://").is_err());
        assert!(Url::parse("http://h:0/").is_err());
        assert!(Url::parse("http://h:99999/").is_err());
        assert!(Url::parse("http://h:port/").is_err());
        assert!(Url::parse("http://user@h/").is_err());
        assert!(Url::parse("http://::1/").is_err());
    }
}
