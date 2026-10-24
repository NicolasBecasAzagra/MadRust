// SPDX-License-Identifier: GPL-2.0-only
//! A small JSON scanner written for the three files this runtime reads.
//!
//! This is deliberately not a general JSON library: it is a cursor over the
//! bytes of a `safetensors` header, a vocabulary map and a config object. It
//! understands the JSON values those files contain, rejects a malformed
//! document with an [`Error`], and never allocates for a value it is skipping.
//!
//! The scanner is a byte cursor. [`Scanner::skip_value`] consumes a complete
//! value without building it, which is what lets the header parser ignore
//! `__metadata__` and any tensor field it does not need. [`Scanner::parse_string`]
//! decodes the JSON escapes, including a surrogate pair, so a vocabulary entry
//! written as `\uXXXX` arrives as the character the file meant.

use crate::error::{Error, Result};

/// A cursor over a JSON document.
pub struct Scanner<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Scanner<'a> {
    /// Start at the first byte.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    /// Offset of the next byte, for diagnostics.
    pub fn position(&self) -> usize {
        self.position
    }

    /// The next byte without consuming it.
    pub fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    /// Consume and return the next byte.
    pub fn bump(&mut self) -> Result<u8> {
        let byte = self
            .peek()
            .ok_or_else(|| Error::new("json: unexpected end of document"))?;
        self.position += 1;
        Ok(byte)
    }

    /// Skip spaces, tabs, carriage returns and newlines.
    pub fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            if matches!(byte, b' ' | b'\t' | b'\r' | b'\n') {
                self.position += 1;
            } else {
                break;
            }
        }
    }

    /// Consume one expected byte.
    pub fn expect(&mut self, expected: u8) -> Result<()> {
        let byte = self.bump()?;
        if byte == expected {
            Ok(())
        } else {
            Err(Error::new(format!(
                "json: expected '{}' at byte {}, found '{}'",
                expected as char, self.position, byte as char
            )))
        }
    }

    /// `true` when the cursor has consumed the whole document.
    pub fn at_end(&mut self) -> bool {
        self.skip_whitespace();
        self.position >= self.bytes.len()
    }

    /// Parse a quoted string and decode its escapes.
    pub fn parse_string(&mut self) -> Result<String> {
        let mut out = Vec::new();
        self.parse_string_into(&mut out)?;
        String::from_utf8(out)
            .map_err(|_| Error::new("json: string is not valid UTF-8".to_string()))
    }

    /// Parse a quoted string into a reusable buffer.
    ///
    /// The vocabulary and the tensor directory keep thousands of strings alive,
    /// so they decode into one buffer at a time instead of allocating a
    /// `String` for every entry. A kernel with a fixed user mapping table must
    /// not spend one mapping per name.
    pub fn parse_string_into(&mut self, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        self.expect(b'"')?;
        loop {
            let byte = self.bump()?;
            match byte {
                b'"' => break,
                b'\\' => self.parse_escape(out)?,
                0x00..=0x1f => {
                    return Err(Error::new(format!(
                        "json: control byte {byte} inside a string at byte {}",
                        self.position
                    )))
                }
                _ => out.push(byte),
            }
        }
        Ok(())
    }

    /// Decode one escape sequence into UTF-8 bytes.
    fn parse_escape(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let escape = self.bump()?;
        match escape {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let mut code = self.parse_hex4()?;
                if (0xd800..=0xdbff).contains(&code) {
                    // A high surrogate must be followed by `\u` and a low
                    // surrogate. The pair becomes one scalar value.
                    self.expect(b'\\')?;
                    self.expect(b'u')?;
                    let low = self.parse_hex4()?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(Error::new("json: lone high surrogate in string"));
                    }
                    code = 0x1_0000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                } else if (0xdc00..=0xdfff).contains(&code) {
                    return Err(Error::new("json: lone low surrogate in string"));
                }
                let character = char::from_u32(code)
                    .ok_or_else(|| Error::new("json: invalid unicode escape"))?;
                let mut buffer = [0u8; 4];
                out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            }
            _ => return Err(Error::new(format!("json: unknown escape \\{escape}"))),
        }
        Ok(())
    }

    /// Read four hexadecimal digits.
    fn parse_hex4(&mut self) -> Result<u32> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.bump()?;
            let digit = match byte {
                b'0'..=b'9' => u32::from(byte - b'0'),
                b'a'..=b'f' => u32::from(byte - b'a') + 10,
                b'A'..=b'F' => u32::from(byte - b'A') + 10,
                _ => return Err(Error::new("json: invalid hexadecimal digit")),
            };
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    /// Parse a number and return it as `f64`.
    pub fn parse_number(&mut self) -> Result<f64> {
        self.skip_whitespace();
        let start = self.position;
        while let Some(byte) = self.peek() {
            match byte {
                b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9' => self.position += 1,
                _ => break,
            }
        }
        if start == self.position {
            return Err(Error::new("json: expected a number"));
        }
        let text = core::str::from_utf8(&self.bytes[start..self.position])
            .map_err(|_| Error::new("json: number is not ASCII"))?;
        text.parse::<f64>()
            .map_err(|_| Error::new(format!("json: invalid number {text}")))
    }

    /// Parse a non-negative integer.
    pub fn parse_usize(&mut self) -> Result<usize> {
        let value = self.parse_number()?;
        if !value.is_finite() || value < 0.0 || value.fract() != 0.0 {
            return Err(Error::new(format!(
                "json: {value} is not a non-negative integer"
            )));
        }
        if value > usize::MAX as f64 {
            return Err(Error::new("json: integer does not fit in usize"));
        }
        Ok(value as usize)
    }

    /// Parse a boolean.
    pub fn parse_bool(&mut self) -> Result<bool> {
        self.skip_whitespace();
        match self.peek() {
            Some(b't') => {
                self.literal(b"true")?;
                Ok(true)
            }
            Some(b'f') => {
                self.literal(b"false")?;
                Ok(false)
            }
            _ => Err(Error::new("json: expected a boolean")),
        }
    }

    /// Consume an exact literal.
    fn literal(&mut self, literal: &[u8]) -> Result<()> {
        for &expected in literal {
            self.expect(expected)?;
        }
        Ok(())
    }

    /// Consume one complete JSON value without building it.
    pub fn skip_value(&mut self) -> Result<()> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => {
                self.parse_string()?;
            }
            Some(b'{') => {
                self.position += 1;
                self.skip_whitespace();
                if self.peek() == Some(b'}') {
                    self.position += 1;
                    return Ok(());
                }
                loop {
                    self.skip_whitespace();
                    self.parse_string()?;
                    self.skip_whitespace();
                    self.expect(b':')?;
                    self.skip_value()?;
                    self.skip_whitespace();
                    match self.bump()? {
                        b',' => continue,
                        b'}' => return Ok(()),
                        _ => return Err(Error::new("json: expected ',' or '}'")),
                    }
                }
            }
            Some(b'[') => {
                self.position += 1;
                self.skip_whitespace();
                if self.peek() == Some(b']') {
                    self.position += 1;
                    return Ok(());
                }
                loop {
                    self.skip_value()?;
                    self.skip_whitespace();
                    match self.bump()? {
                        b',' => continue,
                        b']' => return Ok(()),
                        _ => return Err(Error::new("json: expected ',' or ']'")),
                    }
                }
            }
            Some(b't') => {
                self.literal(b"true")?;
            }
            Some(b'f') => {
                self.literal(b"false")?;
            }
            Some(b'n') => {
                self.literal(b"null")?;
            }
            Some(_) => {
                self.parse_number()?;
            }
            None => return Err(Error::new("json: unexpected end of document")),
        }
        Ok(())
    }

    /// Parse an array of non-negative integers.
    pub fn parse_usize_array(&mut self) -> Result<Vec<usize>> {
        let mut values = Vec::new();
        self.parse_usize_array_into(&mut values)?;
        Ok(values)
    }

    /// Parse an array of non-negative integers into a reusable buffer.
    pub fn parse_usize_array_into(&mut self, values: &mut Vec<usize>) -> Result<()> {
        values.clear();
        self.skip_whitespace();
        self.expect(b'[')?;
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Ok(());
        }
        loop {
            values.push(self.parse_usize()?);
            self.skip_whitespace();
            match self.bump()? {
                b',' => continue,
                b']' => return Ok(()),
                _ => return Err(Error::new("json: expected ',' or ']' in an array")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Scanner;

    #[test]
    fn parses_plain_and_escaped_strings() {
        let mut scanner = Scanner::new(br#""a\"b\\c\n\u0041""#);
        assert_eq!(scanner.parse_string().unwrap(), "a\"b\\c\nA");
    }

    #[test]
    fn parses_a_surrogate_pair() {
        let mut scanner = Scanner::new(br#""\ud83d\ude00""#);
        assert_eq!(scanner.parse_string().unwrap(), "\u{1f600}");
    }

    #[test]
    fn skips_nested_values() {
        let mut scanner = Scanner::new(br#"{"a":[1,{"b":true},"c"],"d":null}"#);
        scanner.skip_value().unwrap();
        assert!(scanner.at_end());
    }

    #[test]
    fn reads_integers_and_bools() {
        let mut scanner = Scanner::new(b" [12, 34] ");
        assert_eq!(scanner.parse_usize_array().unwrap(), vec![12, 34]);
        let mut scanner = Scanner::new(b"true");
        assert!(scanner.parse_bool().unwrap());
    }

    #[test]
    fn stops_at_a_number_delimiter() {
        let mut scanner = Scanner::new(b"12x");
        assert_eq!(scanner.parse_usize().unwrap(), 12);
        assert_eq!(scanner.position(), 2);
    }
}
